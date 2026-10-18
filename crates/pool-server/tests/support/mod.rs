/// Keep integration test servers within the caller's reserved loopback range.
pub async fn listener() -> tokio::net::TcpListener {
    for port in 18300..18400 {
        match tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await {
            Ok(listener) => return listener,
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => continue,
            Err(error) => panic!("cannot bind test listener: {error}"),
        }
    }
    panic!("no free test listener in 127.0.0.1:18300-18399");
}
