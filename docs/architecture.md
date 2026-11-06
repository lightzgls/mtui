# Kiến trúc và cách tổ chức code MTUI

MTUI tổ chức theo tính năng và ranh giới chạy: giao diện terminal, điều phối ứng
dụng, audio và công việc nền. Một module có thể có nhiều file con; không cần
tạo thêm crate chỉ để chia một file dài.

## Đánh giá code hiện tại

Những nền tảng tốt đã có:

- `ui` vẽ bằng Ratatui; `app` xử lý hành động và trạng thái. Audio thuộc
  `player`, HTTP và subprocess thuộc các worker trong `source`.
- `Request`, `Response`, `Command` và `PlayerEvent` thể hiện giao tiếp giữa
  các phần qua message. Request ID, queue epoch và session generation xử lý
  response đến trễ thay vì giả định thứ tự hoàn thành.
- Queue, lịch sử điều hướng và cache có giới hạn. Đây là những ràng buộc cần
  giữ khi refactor một ứng dụng ưu tiên bộ nhớ thấp.
- Scenario test dùng player/source không hoạt động để điều khiển sự kiện;
  không cần mở audio device hay truy cập tài khoản cho mỗi test điều phối.
- `mtui-resolver` đã có ranh giới crate riêng cho việc phân giải stream.

Trước đợt tổ chức này, `app.rs` có khoảng 7.200 dòng và `ui.rs` gần 7.000 dòng,
bao gồm cả test. Chúng gom nhiều lý do thay đổi vào cùng một file: sửa menu,
seek, đăng nhập, điều hướng hay cách vẽ lyric đều đụng đến file trung tâm.
Việc tách module giúp tìm và sửa đúng phần, nhưng chưa làm `App` trở thành
các state độc lập hoàn toàn.

## Cấu trúc

```text
src/
  main.rs                 Khởi động, vòng lặp foreground/background
  app.rs                  App, khởi tạo dịch vụ, overlay và API dùng chung
  app/
    account.rs            Đăng nhập, gia hạn session, đăng xuất
    actions.rs            Like, share, lưu playlist và hành động bài hát
    artist.rs             State và thao tác trang nghệ sĩ
    artwork.rs            State ảnh, cache và kế hoạch đặt ảnh terminal
    desktop.rs            Tray, media controls và Discord activity
    home.rs               Nạp feed, chọn và mở card trên Home
    input.rs              Phân phối key/mouse thành hành động
    library.rs            Cache, phân trang và thao tác Library
    menu.rs               State menu, danh sách lệnh, thực thi lệnh
    navigation.rs         Route, Back history, tương quan page request
    now_playing.rs        State bài hiện tại và thao tác queue
    playback.rs           Play/seek/stop, recovery, queue và listening
    playback/state.rs     PlaybackState: chuyển trạng thái, seek chờ và listening
    prefetch.rs           PrefetchState: debounce selection và cache knowledge
    preferences.rs        State settings và áp dụng lựa chọn
    responses.rs          Poll và áp dụng response từ source worker
    search.rs             Tìm kiếm, chọn kết quả, dispatch request
    search_input.rs       Sửa chuỗi tìm kiếm theo grapheme
    selection.rs          Nhận diện click/chọn bài
    tests.rs              Unit test của application state
    scenarios.rs          Chuỗi sự kiện với dịch vụ giả lập
    performance.rs        Probe hiệu năng được bật riêng
  ui.rs                   Ghép các vùng của frame và kế hoạch ảnh
  ui/
    actions.rs            Hàng action và dialog bài hát
    artist.rs             Trang nghệ sĩ
    artwork.rs            Fit, crop và vẽ ảnh pixel/ASCII
    browse.rs             Trang album/playlist
    cards.rs              Card dùng chung và hình học shelf
    comments.rs           Panel bình luận
    dialog.rs             Khung dialog dùng chung
    home.rs               Feed và layout Home
    library.rs            Trang Library
    lyrics.rs             Lyric, highlight và vị trí follow
    menu.rs               Vẽ menu
    mouse.rs              Hit test và vị trí control liên tục
    overlays.rs           Overlay thông báo và đăng nhập
    paging.rs             Control phân trang
    palette.rs            Màu từ artwork và màu chữ dễ đọc
    player.rs             Trang Now Playing và ghép panel
    player_split.rs       Panel Queue/Related/Comments phía trên, Lyrics phía dưới
    preferences.rs        Vẽ settings
    queue.rs              Hàng queue và vùng click
    related.rs            Card nhạc liên quan
    results.rs            Cột và viewport danh sách bài
    search.rs             Kết quả tìm kiếm nhiều loại
    shell.rs              Header, vùng nội dung và geometry chung
    sidebar.rs            Cột Now Playing cố định khi duyệt nhạc
    status.rs             Footer, mini player, transport và volume
    text.rs               Đo, cắt, wrap và marquee chữ terminal
    tests.rs              Test layout/render với TestBackend
  player/                 Thread audio, decoder, output, HTTP streaming
  source/                 Provider, parser, worker, cache và journal
  session.rs, session/    Session và giao thức helper
  media.rs, media/        Adapter media controls theo OS
  tray.rs, tray/          Tray và animation
  update.rs, update/      Cập nhật theo nền tảng
  bin/mtui-sign-in.rs      Executable đăng nhập độc lập
crates/mtui-resolver/      Phân giải và kiểm tra stream URL
```

`app.rs` cùng `app/` là cách khai báo module hợp lệ của Rust, tương tự
`ui.rs` cùng `ui/`. Không cần đổi tất cả thành `mod.rs`.

## Quy tắc đặt code

1. Một tính năng có phần **hành vi** trong `app/<feature>.rs` và phần **vẽ**
   trong `ui/<feature>.rs` khi cần. Cùng tên giúp tìm hai phía của một luồng.
2. Hàm định dạng, đo chữ và hình học dùng chung nằm trong module có tên cụ
   thể như `ui/text.rs`, `ui/cards.rs`, `ui/artwork.rs`. Tránh gom mọi thứ vào
   `utils.rs`, `helpers.rs` hoặc một thư mục `common` không có chủ sở hữu.
3. `app` dispatch công việc nền; provider parser và HTTP không đi vào `ui`.
   Worker trả dữ liệu về; không trực tiếp điều khiển giao diện.
4. Import rõ module sở hữu. Dùng `pub(super)` cho chi tiết cần chia sẻ trong
   cùng subsystem; chỉ re-export ở `app.rs`/`ui.rs` những tên cần cho phần khác
   hoặc API nội bộ đang được dùng. Không dùng `include!` để chia file.
5. Test gần tính năng hoặc trong file test riêng của module. Giữ scenario
   test xuyên tính năng ở `app/scenarios.rs`. Không đổi tên test chỉ vì di
   chuyển file, vì runner có lọc theo module.
6. Tạo crate riêng khi có API và dependency độc lập thực sự, nhu cầu tái sử
   dụng hoặc ranh giới build; số dòng tự nó chưa đủ là lý do.

## SOLID trong Rust

SOLID dùng được như nguyên tắc thiết kế. Rust diễn đạt chúng bằng module,
composition, trait và hợp đồng hành vi; không cần dựng cây class.

| Nguyên tắc | Áp dụng cho MTUI |
| --- | --- |
| Single Responsibility | Tách điều hướng, menu, playback, account và render. Bước tiếp theo là thu nhỏ state mà mỗi phần có quyền sửa. |
| Open/Closed | Dùng trait khi có nhiều implementation cần thay thế qua cùng một hợp đồng. Với tập trạng thái/lệnh hữu hạn, enum và exhaustive match vẫn phù hợp. |
| Liskov Substitution | Implementation của cùng trait phải giữ cùng hợp đồng, kể cả lỗi, hủy và thứ tự sự kiện. Compiler kiểm tra type/signature; test vẫn phải kiểm tra hành vi. |
| Interface Segregation | Nếu cần abstraction, chia theo năng lực như playback hoặc tìm kiếm; tránh một trait provider gồm mọi chức năng account, cover, lyric và audio. |
| Dependency Inversion | Message giữa App và worker đã giảm phụ thuộc vào công việc nền. App hiện vẫn giữ `Player` và `SourceWorker` cụ thể; chỉ thêm port/trait khi có nhu cầu thay implementation rõ ràng. |

Trait không tự làm code sạch hơn. Một trait chỉ có một implementation và
không có yêu cầu thay thế có thể thêm chi phí đọc mà chưa đem lại lợi ích.
Ownership cũng không tự bảo đảm phân chia trách nhiệm đúng.

Tham khảo chính thức: [module và privacy](https://doc.rust-lang.org/book/ch07-02-defining-modules-to-control-scope-and-privacy.html),
[trait và hành vi dùng chung](https://doc.rust-lang.org/book/ch10-02-traits.html).

## Những phần còn nên cải thiện

- `App` còn nhiều field phục vụ duyệt trang, account và settings. Phần transport
  đã có `PlaybackState`; có thể tiếp tục gom thành `BrowseState`, `AccountState`
  và giới hạn thao tác qua method. Giữ test stale response/seek/queue cho từng bước.
- Renderer vẫn nhận `&mut App` để cập nhật viewport, selection và kế hoạch
  ảnh. Tách view data chỉ đọc khỏi layout feedback sẽ làm ranh giới chặt hơn.
- `source` còn chứa cả model dùng chung và model theo YouTube. Nếu hỗ trợ
  thêm provider thật, cần xác định model chung và năng lực từng provider trước
  khi tạo abstraction. Feature Spotify hiện là bản phác thảo.
- `playback.rs` còn tương đối lớn; có thể tiếp tục tách recovery và queue
  dispatch khi đã xác định được state sở hữu riêng. Tránh chỉ chia tiếp theo
  số dòng mà vẫn truyền toàn bộ App qua mọi hàm.

## State playback và prefetch

`App` giữ hai state nội bộ với field riêng tư, độc lập với dữ liệu render trong
`NowPlaying`:

- `PlaybackState` sở hữu request resolve đang chờ, deferred seek, recovery,
  lỗi playback, cạnh EOF, ngân sách auto-skip và tiến độ listening. Seek đang
  chờ nằm trong request của đúng bài; response khác ID không thể lấy mất seek.
  Chọn bài mới, Stop hoặc hủy resolve loại bỏ seek cùng request đó.
- `PrefetchState` sở hữu debounce selection, probe đang chạy và ID đã warm.
  Response của queue và selection chỉ giải phóng probe cùng ID; một response
  thất bại cũ không xóa kiến thức cache của bài khác.

State quyết định chuyển trạng thái; `App` vẫn dispatch command, gửi request và
ghi journal. Checkpoint listening chỉ được đánh dấu đã gửi sau khi ghi durable
outbox thành công. Canonical metadata không thay nonce hay số giây đã nghe.
UI tiếp tục dùng `App::now`, snapshot và các API hiện có; không được sửa trực
tiếp các cờ transport hoặc prefetch.

Refactor giữ chính sách playback, format lưu dữ liệu và dependency hiện có.

## Kiểm tra sau refactor

```powershell
./scripts/test.ps1 -Suite Smoke
./scripts/test.ps1 -Suite Full
```

Full chạy toàn workspace/all targets và Clippy với `-D warnings`, dùng profile
dữ liệu riêng và giữ các test live/account/audio được đánh dấu ignored.
Xem [hướng dẫn kiểm thử](testing.md) cho giới hạn của kiểm tra headless và
cách kiểm tra audio, terminal, tray trên hệ điều hành thật.
