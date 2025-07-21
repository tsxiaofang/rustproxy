use lazy_static::lazy_static;
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;
use std::{env, error, fmt, fs::File, io, io::BufRead, io::BufReader};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task;
use tokio_socks::tcp::Socks5Stream;

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
            Self::CommError(v) => write!(f, "{v}"),
            Self::ConnectClosed(v) => write!(f, "{v}"),
            Self::EmptyCommand(v) => write!(f, "{v}"),
            Self::UnknownCommand(v) => write!(f, "{v}"),
            Self::UnknownFormat(v) => write!(f, "{v}"),
            Self::IoError(v) => write!(f, "{v}"),
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

        // file content format
        // key=val\n
        if let Ok(f) = File::open("map.txt") {
            let reader = BufReader::new(f);
            for line in reader.lines().map_while(|v| v.ok()) {
                let vs: Vec<&str> = line.split('=').collect();
                if vs.len() >= 2 {
                    let key = vs[0].trim();
                    let val = vs[1].trim();
                    m.insert(key.into(), val.into());
                }
            }
        }

        // file content format
        // [["ip", "domain"],["ip", "domain"]]
        if let Ok(json_str) = std::fs::read_to_string("hosts.json") {
            if let Ok(hosts) = serde_json::from_str::<Vec<Vec<String>>>(&json_str) {
                for item in hosts {
                    if item.len() >= 2 {
                        let val = item[0].trim();
                        let key = item[1].trim();
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
    for (k, v) in SVR_ADDR_MAP.iter() {
        println!("key:{k}, val:{v}");
    }
    let mut bind_addr = String::from("0.0.0.0:1080");
    let mut target_addr = String::from("proxy");
    let mut socks_addr = String::default();

    for mut arg in env::args() {
        let v = arg.to_lowercase();

        if v.starts_with("-b=") {
            bind_addr = arg.split_off(3);
        } else if v.starts_with("-t=") {
            target_addr = arg.split_off(3);
        } else if v.starts_with("-pos=") {
            socks_addr = arg.split_off(5);
        }
    }

    println!("rustproxy -b={bind_addr} -t={target_addr} -pos={socks_addr}");

    if bind_addr.is_empty() || target_addr.is_empty() {
        return Ok(()); //Err(io::Error::from(io::ErrorKind::InvalidInput));
    }

    let socks_proxy = Arc::new(socks_addr);
    let target_addr = Arc::new(target_addr);
    let listener = TcpListener::bind(bind_addr).await?;

    while let Ok((socket, accept_addr)) = listener.accept().await {
        println!("clinet {accept_addr} connected.");

        let target = target_addr.clone();
        let socks = socks_proxy.clone();

        task::spawn(process_client_handler(socket, target, socks));
    }

    Ok(())
}

//GET http://127.0.0.1:8081/artifactory/api/conan/conan-vrv/v1/ping HTTP/1.1

async fn connect_target(
    fd: &mut TcpStream,
    target_addr: &str,
    socks_proxy: &str,
) -> Result<TcpStream> {
    let mut result = Cow::Borrowed(target_addr);
    let mut buf = [0u8; 4096];
    let mut b_flag = 0;
    let mut url = String::new();

    if result.eq_ignore_ascii_case("proxy") {
        let nread1 = fd.read(&mut buf).await?;
        if nread1 == 0 {
            return Err(AddressError::ConnectClosed("connect closed."));
        }

        let mut http_cmd = String::new();

        let nread2 = (&buf[..]).read_line(&mut http_cmd)?;

        if nread2 == 0 {
            return Err(AddressError::EmptyCommand("empty command."));
        }

        println!("{http_cmd}");

        let v: Vec<&str> = http_cmd.split(' ').collect();
        if v.len() < 3 {
            return Err(AddressError::UnknownFormat("unknown format."));
        }

        match v[0].trim().to_uppercase().as_str() {
            "CONNECT" => {
                b_flag = 1;
                result = Cow::Owned(v[1].to_string());
            }
            "GET" => {
                if let Some(n) = v[1].find("//") {
                    let (_, r) = v[1].split_at(n + 2);

                    if let Some(n) = r.find('/') {
                        let (l, r) = r.split_at(n);
                        b_flag = 2;

                        url.push_str("GET ");
                        url.push_str(r);
                        url.push(' ');
                        url.push_str(v[2]);
                        url.push_str(&String::from_utf8_lossy(&buf[nread2..nread1]));

                        result = Cow::Owned(l.to_string());
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

        if !result.contains(':') {
            result = Cow::Owned(format!("{result}:80"));
        }

        if let Some(val) = SVR_ADDR_MAP.get(result.as_ref()) {
            result = Cow::Borrowed(val);
        } else if let Some((l, r)) = result.split_once(':') {
            if let Some(val) = SVR_ADDR_MAP.get(l) {
                result = Cow::Owned(format!("{val}:{r}"));
            }
        }

        println!("proxy connect {result}");

        if b_flag == 1 {
            fd.write_all(b"HTTP/1.1 200 Connection established\r\nHost: Rust Proxy\r\nConnection: keep-alive\r\nContent-Length: 0\r\n\r\n").await?;
        }
    }

    match socks_proxy.is_empty() {
        true => {
            let mut s_conn = TcpStream::connect(result.as_ref()).await?;
            if b_flag == 2 && !url.is_empty() {
                s_conn.write_all(url.as_bytes()).await?;
            }
            Ok(s_conn)
        }
        false => {
            let s = Socks5Stream::connect(socks_proxy, result.as_ref())
                .await
                .map_err(|_| AddressError::CommError("connect proxy socks error."))?;
            Ok(s.into_inner())
        }
    }
}

async fn process_client_handler(
    mut s_client: TcpStream,
    target_addr: Arc<String>,
    socks_proxy: Arc<String>,
) -> Result<()> {
    let c_addr = s_client.peer_addr();

    let s_conn = connect_target(&mut s_client, &target_addr, &socks_proxy).await;

    let mut t_text = "target unreachable";

    if let Ok(s_server) = s_conn {
        let (mut s_rx, mut s_tx) = s_server.into_split();
        let (mut c_rx, mut c_tx) = s_client.into_split();

        let f1 = tokio::io::copy(&mut s_rx, &mut c_tx);
        let f2 = tokio::io::copy(&mut c_rx, &mut s_tx);

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
        println!("{t_text} connection {addr}.");
    }

    Ok(())
}
