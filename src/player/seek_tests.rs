//! Seek regressions cover stream interruption as well as progress-bar endpoints.

use super::*;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1).enable_all().build().unwrap()
}

#[test]
fn playback_commands_interrupt_a_seek_without_waiting_for_the_worker() {
    let runtime = runtime();
    for command in [
        Command::Seek(Duration::from_secs(120)),
        Command::SeekToEnd(Duration::from_secs(240)),
        Command::Load { title: "Another song".into() },
        Command::Play { id: "next".into(), title: "Next".into(), url: "http://localhost/next".into(), format: AudioFormat { itag: None } },
        Command::Stop,
        Command::Shutdown,
    ] {
        let (player, driver) = Player::for_test();
        let link = StreamLink::default();
        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = cancelled.clone();
        link.set_cancellation(move || flag.store(true, Ordering::SeqCst));
        *player.active_stream.lock().unwrap() = Some(link.clone());
        let guard = link.begin_seek(&runtime, SEEK_TIMEOUT);
        player.send(command).unwrap();
        assert!(cancelled.load(Ordering::SeqCst), "the worker must not have to receive the command to cancel a blocked read");
        assert!(driver.commands.try_recv().is_ok());
        assert!(!link.seek_timed_out());
        drop(guard);
    }
}

#[test]
fn cancelling_a_seek_releases_an_actual_stream_download_read() {
    let mut reply = b"HTTP/1.1 200 OK\r\nContent-Length: 65536\r\nConnection: close\r\n\r\n".to_vec();
    reply.resize(reply.len() + 32768, 1);
    let (url, server) = crate::source::test_http::serve(vec![(reply, Duration::from_secs(2))]);
    let runtime = runtime();
    let (mut stream, link) = runtime.block_on(backend::open(&url)).unwrap();
    let (ready_tx, ready) = channel();
    let (done_tx, done) = channel();
    let reader = thread::spawn(move || {
        ready_tx.send(()).unwrap();
        done_tx.send(stream.read(&mut [0; 65536])).unwrap();
    });
    ready.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(done.recv_timeout(Duration::from_millis(50)).is_err(), "the fixture must have a blocked read before cancellation");
    let guard = link.begin_seek(&runtime, Duration::from_millis(50));
    let result = done.recv_timeout(Duration::from_secs(1)).expect("a seek timeout must wake a blocked decoder read");
    assert!(result.is_err());
    assert!(link.seek_timed_out());
    drop(guard);
    reader.join().unwrap();
    server.join().unwrap();
}

#[test]
fn completing_a_seek_disarms_its_timeout() {
    let runtime = runtime();
    let link = StreamLink::default();
    let guard = link.begin_seek(&runtime, Duration::from_millis(30));
    drop(guard);
    runtime.block_on(async { tokio::time::sleep(Duration::from_millis(80)).await });
    assert!(!link.cancelled());
    assert!(!link.seek_timed_out());
    assert!(!link.seeking());
}

#[test]
fn shutdown_interrupts_stream_opening_before_any_audio_arrives() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/waiting", listener.local_addr().unwrap());
    let (ready_tx, ready) = channel();
    let server = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        assert!(socket.read(&mut [0; 4096]).unwrap() > 0);
        ready_tx.send(()).unwrap();
        let _ = socket.read(&mut [0; 1]);
    });
    let (player, _driver) = Player::for_test();
    let active_stream = player.active_stream.clone();
    let (done_tx, done) = channel();
    let worker = thread::spawn(move || {
        let runtime = PlaybackRuntime { runtime: runtime(), active_stream };
        done_tx.send(open_stream(&runtime, &url, false).map(|_| ())).unwrap();
    });
    ready.recv_timeout(Duration::from_secs(2)).unwrap();
    player.send(Command::Shutdown).unwrap();
    assert!(done.recv_timeout(Duration::from_secs(1)).expect("Quit must interrupt opening, not wait for the HTTP timeout").is_err());
    worker.join().unwrap();
    server.join().unwrap();
}

#[test]
#[ignore = "uses a local AAC fixture and an audio output at volume 0; localhost only"]
fn stalled_middle_seeks_allow_another_song_stop_and_shutdown_in_the_same_session() {
    let audio = std::fs::read(std::env::var_os("MTUI_AUDIO_FIXTURE").expect("set MTUI_AUDIO_FIXTURE")).unwrap();
    assert!(audio.len() > 65536);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let done = Arc::new(AtomicBool::new(false));
    let server_done = done.clone();
    let deny_head = Arc::new(AtomicBool::new(false));
    let server_deny_head = deny_head.clone();
    let server = thread::spawn(move || {
        while !server_done.load(Ordering::Relaxed) {
            let Ok((mut socket, _)) = listener.accept() else { thread::sleep(Duration::from_millis(5)); continue; };
            socket.set_nonblocking(false).unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
            socket.set_write_timeout(Some(Duration::from_secs(1))).unwrap();
            let mut request = [0; 4096];
            let read = socket.read(&mut request).unwrap_or(0);
            let request = String::from_utf8_lossy(&request[..read]).to_ascii_lowercase();
            let available = if request.contains("/capped") { 65536 } else { audio.len() };
            let (start, end) = request.lines().find_map(|line| line.strip_prefix("range: bytes="))
                .and_then(|range| range.split_once('-'))
                .map(|(start, end)| (start.parse::<usize>().unwrap(), end.parse::<usize>().unwrap_or(audio.len() - 1)))
                .unwrap_or((0, audio.len() - 1));
            if start >= available || (request.contains("/capped") && server_deny_head.load(Ordering::Relaxed)) {
                let _ = socket.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            } else {
                let end = end.min(available - 1);
                let _ = write!(socket, "HTTP/1.1 206 Partial Content\r\nContent-Type: audio/mp4\r\nContent-Length: {}\r\nContent-Range: bytes {start}-{end}/{}\r\nConnection: close\r\n\r\n", end - start + 1, audio.len());
                let _ = socket.write_all(&audio[start..=end]);
            }
        }
    });

    let player = Player::spawn(0.0, None).unwrap();
    let play = |path: &str, id: &str| {
        player.send(Command::Load { title: id.into() }).unwrap();
        player.send(Command::Play { url: format!("{base}/{path}"), format: AudioFormat { itag: None }, title: id.into(), id: id.into() }).unwrap();
    };
    let wait = |predicate: &dyn Fn() -> bool| {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !predicate() && Instant::now() < deadline { thread::sleep(Duration::from_millis(10)); }
        assert!(predicate(), "player did not respond: {:?}", player.snapshot());
    };
    let stall = || {
        play("capped", "capped");
        wait(&|| player.snapshot().position >= Duration::from_millis(100));
        player.send(Command::Seek(Duration::from_secs(8))).unwrap();
        wait(&|| player.active_stream.lock().unwrap().as_ref()
            .is_some_and(|link| link.seeking() && link.wants_url().is_some()));
    };

    // A real opening failure after stopping the old download is an error,
    // not an interruption that leaves the player buffering indefinitely.
    play("capped", "capped");
    wait(&|| player.snapshot().position >= Duration::from_millis(100));
    deny_head.store(true, Ordering::Relaxed);
    player.send(Command::Seek(Duration::from_secs(8))).unwrap();
    wait(&|| player.snapshot().state == PlayState::Idle && player.snapshot().error.is_some());
    deny_head.store(false, Ordering::Relaxed);
    play("valid", "after-open-error");
    wait(&|| player.snapshot().title == "after-open-error" && player.snapshot().position >= Duration::from_millis(100));

    // Another scrub position must win without turning cancellation into EOF.
    stall();
    player.send(Command::Seek(Duration::from_secs(1))).unwrap();
    wait(&|| player.snapshot().state == PlayState::Playing && player.snapshot().position >= Duration::from_secs(1));
    assert!(player.snapshot().error.is_none());

    // A different representation arriving during the seek restarts its
    // preparation, rather than waiting for the very worker it needs to free.
    stall();
    player.send(Command::ArmReplacement { id: "capped".into(), url: format!("{base}/valid"), format: AudioFormat { itag: Some(140) } }).unwrap();
    wait(&|| player.snapshot().state == PlayState::Playing && player.snapshot().position >= Duration::from_secs(8));
    assert!(player.snapshot().error.is_none());

    // A newer track must cancel the seek, not wait for its recovery URL.
    stall();
    play("valid", "next");
    wait(&|| player.snapshot().title == "next" && player.snapshot().position >= Duration::from_millis(100));

    // An endpoint arriving after an intermediate scrub also interrupts it.
    stall();
    player.send(Command::SeekToEnd(Duration::from_secs(240))).unwrap();
    wait(&|| player.snapshot().state == PlayState::Idle);
    play("valid", "after-end");
    wait(&|| player.snapshot().title == "after-end" && player.snapshot().position >= Duration::from_millis(100));

    stall();
    player.send(Command::Stop).unwrap();
    wait(&|| player.snapshot().state == PlayState::Idle && player.snapshot().title.is_empty());
    play("valid", "after-stop");
    wait(&|| player.snapshot().title == "after-stop" && player.snapshot().position >= Duration::from_millis(100));

    player.send(Command::TogglePause).unwrap();
    wait(&|| player.snapshot().state == PlayState::Paused);
    player.send(Command::Seek(Duration::from_secs(1))).unwrap();
    wait(&|| player.snapshot().state == PlayState::Paused && player.snapshot().position == Duration::from_secs(1));
    player.send(Command::SeekToEnd(Duration::from_secs(240))).unwrap();
    wait(&|| player.snapshot().state == PlayState::Paused && player.snapshot().position > Duration::from_secs(1));
    player.send(Command::Seek(Duration::ZERO)).unwrap();
    wait(&|| player.snapshot().state == PlayState::Paused && player.snapshot().position.is_zero());
    player.send(Command::TogglePause).unwrap();
    wait(&|| player.snapshot().state == PlayState::Playing && player.snapshot().position >= Duration::from_millis(100));

    // An unanswered seek has a finite deadline; its failure must not poison
    // future plays in this process.
    stall();
    let deadline = Instant::now() + Duration::from_secs(7);
    while player.snapshot().error.is_none() && Instant::now() < deadline { thread::sleep(Duration::from_millis(10)); }
    assert_eq!(player.snapshot().state, PlayState::Idle);
    assert!(player.snapshot().error.as_deref().is_some_and(|error| error.contains("seek timed out")));
    play("valid", "after-timeout");
    wait(&|| player.snapshot().title == "after-timeout" && player.snapshot().position >= Duration::from_millis(100));

    stall();
    let (closed_tx, closed) = channel();
    thread::spawn(move || { drop(player); let _ = closed_tx.send(()); });
    assert!(closed.recv_timeout(Duration::from_secs(2)).is_ok(), "Quit must join the player even during a stalled seek");
    done.store(true, Ordering::Relaxed);
    server.join().unwrap();
}
