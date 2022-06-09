use std::collections::HashMap;
use std::{env, error, fmt, fs::File, io, io::BufRead, io::BufReader};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::task;

#[macro_use]
extern crate lazy_static;

#[allow(unused)]
#[derive(Debug)]
enum AddressError {
    CommError(&'static str),
    ConnectClosed(&'static str),
    EmptyCommand(&'static str),
    UnknownCommand(&'static str),
    UnknownFormat(&'static str),
    IoError(std::io::Error),
}

impl fmt::Display for AddressError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CommError(v) => write!(f, "{}", v),
            Self::ConnectClosed(v) => write!(f, "{}", v),
            Self::EmptyCommand(v) => write!(f, "{}", v),
            Self::UnknownCommand(v) => write!(f, "{}", v),
            Self::UnknownFormat(v) => write!(f, "{}", v),
            Self::IoError(v) => write!(f, "{}", v),
        }
    }
}

impl error::Error for AddressError {}

impl From<io::Error> for AddressError {
    fn from(t: io::Error) -> Self {
        Self::IoError(t)
    }
}

type Result<T> = std::result::Result<T, AddressError>;

lazy_static! {
    static ref SVR_ADDR_MAP: HashMap<String, String> = {
        let mut m = HashMap::new();

        if let Ok(f) = File::open("map.txt") {
            let reader = BufReader::new(f);
            for line in reader.lines() {
                if let Ok(l) = line {
                    let vs: Vec<&str> = l.split("=").collect();
                    if vs.len() >= 2 {
                        let key = vs[0].trim();
                        let val = vs[1].trim();
                        m.insert(key.into(), val.into());
                    }
                }
            }
        }

        m
    };
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut bind_addr = String::from("0.0.0.0:1080");
    let mut target_addr = String::from("proxy");

    for mut arg in env::args() {
        let v = arg.to_lowercase();

        if v.starts_with("-b=") {
            bind_addr = arg.split_off(3);
        } else if v.starts_with("-t=") {
            target_addr = arg.split_off(3);
        }
    }

    println!("rustproxy -b={} -t={}", bind_addr, target_addr);

    if bind_addr.is_empty() || target_addr.is_empty() {
        return Ok(()); //Err(io::Error::from(io::ErrorKind::InvalidInput));
    }

    let listener = TcpListener::bind(bind_addr).await?;

    while let Ok((socket, accept_addr)) = listener.accept().await {
        println!("clinet {} connected.", accept_addr.to_string());

        let target = target_addr.clone();

        task::spawn(process_client_handler(socket, target));
    }

    Ok(())
}

//GET http://127.0.0.1:8081/artifactory/api/conan/conan-vrv/v1/ping HTTP/1.1

async fn connect_target(fd: &mut TcpStream, target_addr: String) -> Result<TcpStream> {
    let mut result = target_addr;
    let mut buf = [0u8; 4096];
    let mut b_flag = 0;
    let mut url = String::new();

    if result == "proxy" {
        let nread1 = fd.read(&mut buf).await?;
        if nread1 <= 0 {
            return Err(AddressError::ConnectClosed("connect closed."));
        }

        let mut http_cmd = String::new();

        let nread2 = (&buf[..]).read_line(&mut http_cmd)?;

        if nread2 <= 0 {
            return Err(AddressError::EmptyCommand("empty command."));
        }

        println!("{http_cmd}");

        let v: Vec<&str> = http_cmd.split(" ").collect();
        if v.len() < 3 {
            return Err(AddressError::UnknownFormat("unknown format."));
        }

        match v[0].trim().to_uppercase().as_str() {
            "CONNECT" => {
                b_flag = 1;
                result = v[1].to_string();
            }
            "GET" => {
                if let Some(n) = v[1].find("//") {
                    let (_, r) = v[1].split_at(n + 2);

                    if let Some(n) = r.find("/") {
                        let (l, r) = r.split_at(n);
                        b_flag = 2;

                        url.push_str("GET ");
                        url.push_str(r);
                        url.push_str(" ");
                        url.push_str(v[2]);
                        url.push_str(&String::from_utf8_lossy(&buf[nread2..nread1]));

                        result = l.to_string();
                    }
                }
            }
            _ => {
                return Err(AddressError::UnknownCommand("unknown command."));
            }
        }

        if b_flag == 0 {
            return Err(AddressError::UnknownCommand("unknown command."));
        }

        if !result.contains(":") {
            result.push_str(":80");
        }

        if let Some(val) = SVR_ADDR_MAP.get(&result) {
            result = val.clone();
        }

        println!("proxy connect {}", result);

        if b_flag == 1 {
            fd.write_all(b"HTTP/1.1 200 Connection established\r\nHost: Rust Proxy\r\nConnection: keep-alive\r\nContent-Length: 0\r\n\r\n").await?;
        }
    }

    let mut s_conn = TcpStream::connect(result).await?;

    if b_flag == 2 && !url.is_empty() {
        s_conn.write_all(url.as_bytes()).await?;
    }

    Ok(s_conn)
}

async fn process_client_handler(mut s_client: TcpStream, target_addr: String) -> Result<()> {
    let c_addr = s_client.peer_addr();

    let s_conn = connect_target(&mut s_client, target_addr).await;

    let mut t_text = "target unreachable";

    if let Ok(s_server) = s_conn {
        let (s_rx, s_tx) = s_server.into_split();
        let (c_rx, c_tx) = s_client.into_split();

        let f1 = transfer_data(s_rx, c_tx);
        let f2 = transfer_data(c_rx, s_tx);

        //tokio::try_join!(f1, f2)?;
        tokio::select! {
            _ = f1 => {
                t_text = "server closed";
            },
            _ = f2 => {
                t_text = "client closed";
            }
        };
    }

    if let Ok(addr) = c_addr {
        println!("{} connection {}.", t_text, addr.to_string());
    }

    Ok(())
}

async fn transfer_data(mut c: OwnedReadHalf, mut s: OwnedWriteHalf) -> io::Result<()> {
    let mut buf = [0u8; 4096];

    while let Ok(nread) = c.read(&mut buf).await {
        if nread == 0 {
            break;
        }

        if let Err(_) = s.write_all(&buf[0..nread]).await {
            break;
        }
    }

    Ok(())
}
