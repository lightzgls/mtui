//! Loopback fixtures only; no provider requests or account mutations.
use std::{io::{Read, Write}, net::TcpListener, thread, time::{Duration, Instant}};

pub(crate) fn serve(replies: Vec<(Vec<u8>, Duration)>) -> (String, thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}/fixture", listener.local_addr().unwrap());
    let worker = thread::spawn(move || {
        let mut requests = Vec::new();
        for (reply, hold) in replies {
            let deadline = Instant::now() + Duration::from_secs(3);
            let (mut socket, request) = loop {
                let mut socket = match listener.accept() {
                    Ok((socket, _)) => socket,
                    Err(_) if Instant::now() < deadline => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(_) => return requests,
                };
                // Accepted sockets inherit the listener's nonblocking mode
                // on some platforms. Reads must wait for incoming headers.
                socket.set_nonblocking(false).unwrap();
                socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                socket.set_write_timeout(Some(Duration::from_secs(2))).unwrap();
                let mut request = Vec::new();
                let mut bytes = [0; 4096];
                while request.len() < 16 * 1024 && !request.windows(4).any(|bytes|bytes == b"\r\n\r\n") {
                    let count = socket.read(&mut bytes).unwrap_or(0);
                    if count == 0 { break; }
                    request.extend_from_slice(&bytes[..count]);
                }
                // A client may open and cancel a connection before sending
                // HTTP. It must not consume a scripted response or count as a
                // retry; wait for a complete request within the same deadline.
                if request.windows(4).any(|bytes|bytes == b"\r\n\r\n") {
                    break (socket, request);
                }
                if Instant::now() >= deadline { return requests; }
            };
            requests.push(String::from_utf8_lossy(&request).to_string());
            let _ = socket.write_all(&reply);
            thread::sleep(hold);
            let _ = socket.shutdown(std::net::Shutdown::Write);
        }
        requests
    });
    (url, worker)
}

pub(crate) fn immediate(reply: &[u8]) -> (Vec<u8>, Duration) { (reply.to_vec(), Duration::ZERO) }

#[test]
fn cancelled_tcp_connections_do_not_consume_http_replies() {
    let (url, server) = serve(vec![immediate(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK")]);
    let port = reqwest::Url::parse(&url).unwrap().port().unwrap();
    let address = format!("127.0.0.1:{port}");
    drop(std::net::TcpStream::connect(&address).unwrap());
    let mut socket = std::net::TcpStream::connect(&address).unwrap();
    socket.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    thread::sleep(Duration::from_millis(50));
    socket.write_all(b"GET /fixture HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").unwrap();
    let mut response = Vec::new();
    socket.read_to_end(&mut response).unwrap();
    assert!(response.ends_with(b"\r\n\r\nOK"));
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with("GET /fixture HTTP/1.1"));
}
