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
            let mut socket = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
                    Err(_) => return requests,
                }
            };
            socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            socket.set_write_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut request = Vec::new();
            let mut bytes = [0; 4096];
            while request.len() < 16 * 1024 && !request.windows(4).any(|bytes|bytes == b"\r\n\r\n") {
                let count = socket.read(&mut bytes).unwrap_or(0);
                if count == 0 { break; }
                request.extend_from_slice(&bytes[..count]);
            }
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
