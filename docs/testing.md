# Kiểm thử MTUI

Mục tiêu là tìm lỗi qua **chuỗi thao tác và thứ tự sự kiện**, bên cạnh kiểm tra
từng chức năng riêng lẻ. Không coi số test pass là bằng chứng đã tìm hết bug.
Các regression tự động nằm trong source; lưu kết quả từng lần chạy trong
`target/test-runs/` hoặc issue tương ứng.

## Chạy bộ kiểm tra

Chạy từ thư mục dự án bằng PowerShell 5.1 hoặc PowerShell 7:

```powershell
# Luồng thao tác, seek/recovery, session và thành phần giao diện
./scripts/test.ps1 -Suite Smoke

# Toàn workspace, tất cả target, rồi Clippy
./scripts/test.ps1 -Suite Full

# Chạy toàn bộ test nhiều lần để tìm lỗi phụ thuộc timing/thread
./scripts/test.ps1 -Suite Stress -Iterations 10 -Threads 8

# Ba probe AAC trên localhost: decoder, recovery/Stop và seek bị kẹt, volume 0
./scripts/test.ps1 -Suite Audio -AudioFixture ./target/playback-regression.m4a
```

Mặc định dùng dependency đã có trên máy và Cargo.lock. Nếu máy mới chưa tải
dependency, thêm `-Online`. Cờ này cho phép Cargo tải dependency; **không bật
các test `#[ignore]`**. CI chạy Full trên Windows, Linux và macOS.
Suite Audio chỉ chạy đúng ba test AAC local được chọn sẵn. Nó cần file fixture
có sẵn; không tải audio. Probe recovery mở output device với volume 0.

Mỗi lần chạy tạo `target/test-runs/<timestamp>-<id>/`, gồm:

- `summary.json`: commit, working tree có thay đổi hay không, hệ điều hành,
  thời điểm, số thread, từng bước kiểm tra, exit code và số test.
- `inventory.log`, `ignored-inventory.log`: tên test hiện có và test chưa chạy.
- `tests-*.log` / `smoke-*.log`, `clippy.log`: bằng chứng của từng bước.
- Profile cấu hình, cache và thư mục tạm riêng cho lần chạy. Biến môi trường
  của process được khôi phục sau khi chạy; runner không xóa dữ liệu người dùng.

Exit code khác 0 nếu bất kỳ bước nào lỗi. Inventory có số pass bằng 0 vì chỉ
liệt kê test. Số pass của Stress là số **lượt chạy**, không phải số test khác nhau.
Profile riêng không cô lập phần cứng, PATH hay mạng của process.

Nếu sandbox chặn HTTP loopback hoặc thư mục tạm, ghi nhận là lỗi môi trường;
chạy lại trong môi trường cho phép localhost. Không bỏ test để biến kết quả đỏ
thành xanh. `--offline` chỉ áp dụng cho Cargo; test loopback vẫn dùng socket.

## Kiểm tra cài đặt và cập nhật Linux

Chạy ShellCheck và bộ giả lập installer trong môi trường có Bash:

```sh
shellcheck scripts/install-linux.sh scripts/test-linux-installer.sh scripts/test-linux-release.sh scripts/package-linux.sh
bash scripts/test-linux-installer.sh
```

Bộ giả lập kiểm tra APT/DNF/pacman/Zypper, Debian trước/sau chuyển đổi time64,
release mới nhất và bản ghim, checksum sai, download lỗi, từ chối dependency,
file trùng với cài đặt khác, distro chưa hỗ trợ, ARM/musl và luồng `curl | bash`.
Package manager được giả lập: không cài package, không thay đổi dữ liệu trên host.
ShellCheck mới có thể báo SC2317 cho mock được gọi gián tiếp qua mảng command;
chỉ bỏ qua cảnh báo tại đúng mock, không bỏ qua toàn bộ kiểm tra script.

Kiểm tra release thật **chỉ trong container dùng một lần**, không chạy trực tiếp
`scripts/test-linux-release.sh` trên desktop. Script cài runtime và app vào HOME
trong container. Thư mục `dist` phải chứa archive, `install.sh` và `SHA256SUMS`
của cùng phiên bản. Ví dụ:

```sh
docker run --rm \
  --mount "type=bind,source=$PWD/scripts,target=/source/scripts,readonly" \
  --mount "type=bind,source=$PWD/dist,target=/packages,readonly" \
  ubuntu:24.04 bash /source/scripts/test-linux-release.sh 0.11.4 /packages
```

Ma trận release gồm Ubuntu 24.04, Fedora 44, Arch và openSUSE Tumbleweed. Kiểm
tra dependency thật, loader của cả hai executable, launcher/icon, helper sign-in
với argument không hợp lệ, không có Rust/compiler, và đường thay thế của updater
thật với fixture `/bin/true`. Fixture không mở Google hoặc phát âm thanh.
Kiểm tra thêm bằng user thường khi thay đổi quyền/path; thư viện đã có thì
installer không được yêu cầu root hoặc gọi package manager. Trên Arch thiếu thư
viện phải dùng `pacman -Syu`, không tạo partial upgrade bằng `pacman -Sy`.

Regression updater phải bao gồm archive thiếu/trùng helper, symlink/path không
hợp lệ, body/checksum sai, ELF không tương thích, thư mục không có quyền ghi,
rollback cả hai executable, và giữ nguyên settings/session/cache. Desktop test
phải xác nhận terminal vẫn nhận input sau restart. Package do root quản lý phải
báo dùng package manager, không tự sửa `/usr/bin`. Linux v0.11.3 ban đầu cần cài
thủ công một lần; bản refresh có thể cập nhật lên v0.11.4 trong app.

CI chạy kiểm tra script riêng, Full/Clippy trên ba OS, và kiểm tra đóng gói Linux.
Release chỉ publish sau build Windows/Linux và bốn smoke job distro. Kiểm tra
Windows bằng `dumpbin`: không phụ thuộc `WebView2Loader.dll`, `VCRUNTIME*.dll`
hoặc `MSVCP*.dll`; vẫn cần WebView2 của OS cho sign-in. Kiểm tra `.deb` mới nâng
cấp được bản cũ và checksum của tất cả asset. Xem [quy trình release](releasing.md).
Headless test pass **không xác nhận** Google login bằng GUI, audio hardware hay
tray. Không chạy account-write/live test ngoài tài khoản và playlist test.

## Chính sách cho ca vừa chọn bài rồi click đến cuối

Khi duration đã biết, giữ **vị trí seek cuối cùng của bài đang được chọn** trong
lúc resolve URL. Áp dụng nó sau Play. Chọn bài khác, Stop hoặc resolve thất bại
phải xóa seek đó. Với duration chưa biết, không tạo một mốc cuối giả.

Seek không tự thay đổi trạng thái pause. Khi bài đang phát kết thúc:

| Queue / repeat | Kết quả cần có |
| --- | --- |
| Repeat Off, còn bài | Sang đúng một bài kế tiếp |
| Repeat Off, hết queue | Dừng; không phát lại bài cũ |
| Repeat All, cuối queue | Quay về đầu queue đúng một lần |
| Repeat One | Phát lại chính bài đó đúng một lần |
| Đang chờ continuation | Chờ có giới hạn; response phải đúng queue epoch |
| Đã Stop / đã chọn bài khác | Response cũ không được khởi động lại bài cũ |
| Đang Paused | Giữ pause; chỉ tiếp tục sau lệnh resume |

Kiểm tra cả trường hợp decoder kết thúc trước khi giao diện thấy một frame
Playing. Buffering → Idle không lỗi cũng có thể là một lần hoàn thành bài.
Buffering → Idle có lỗi phải được báo lỗi, không bị hiểu thành kết thúc bình thường.

## Các lớp kiểm thử

1. **State và command:** `src/app/scenarios.rs` dùng player/source giả lập để
   điều khiển snapshot, response và event theo thứ tự xác định. Không tạo audio
   device, network worker, WebView hay Discord worker. Test gọi đường xử lý
   click/key và ứng dụng response của App thật; nó không xác nhận âm thanh thật.
2. **Component và HTTP loopback:** các test đang có cho decoder packets,
   range/retry, giới hạn body, parser, session, journal, config và Ratatui.
   Fixture HTTP chỉ trỏ localhost; account-write dùng callback giả lập.
3. **Audio/Windows tích hợp:** AAC thật, thiết bị thật, console/tray, sleep/wake,
   resize lúc drag và nghe để đối chiếu clock/lyrics. Ghi thao tác, thiết bị,
   terminal, thời gian và kết quả riêng.
4. **Provider/account trực tiếp:** kiểm tra nội dung/search/account thật khi có
   điều kiện. Dùng tài khoản test và playlist test cho ca ghi dữ liệu. Các test
   history có thể ghi lên tài khoản đang đăng nhập, nên không chạy cả danh sách
   ignored bằng một lệnh.

Các test ignored được giữ nguyên trừ hai regression UI/actions đã được đổi sang
fixture thuần offline. Preview, live API, audio thật và history write trong
inventory **không có nghĩa đã pass**.

Regression seek dùng stream AAC localhost bị từ chối sau 64 KiB. Trong lúc seek
đến giữa bài đang chờ byte, kiểm tra chọn bài khác, seek lại, Stop và Quit. Cùng
một player phải phát được bài kế tiếp; Quit phải kết thúc trong thời gian hữu hạn.

## Dữ liệu và môi trường cần chuẩn bị cho các ca tích hợp

| Nhóm | Bộ dữ liệu / biến thể |
| --- | --- |
| Bài hát | AAC bình thường, 1 giây, dài trên 1 giờ, live, duration thiếu/0/sai; bài đã xóa, private, region-limited |
| Queue | Rỗng, 1 bài, 2 bài, 86 bài, trang 60 bài, duplicate ID, không còn bài mới; playlist lớn |
| Response | Đến ngay, trễ 1/5/30 giây, A trước B, B trước A, duplicate, lỗi rồi thành công, response sau Stop/logout |
| Seek | 0%, 1%, 50%, 99.9%, 100%, điểm ngoài progress bar; lần cuối thắng |
| UI | 0×0/1×1 để kiểm tra không panic, 24×8/48×18/64×20 để kiểm tra thu nhỏ; 100×36, 160×42, 216×50 |
| Text | Tiếng Việt có dấu, dấu tổ hợp, CJK, emoji ZWJ/skin tone/flag, RTL, title dài, metadata thiếu |
| Network | 401/403/404/416/429/500/503, DNS/TLS lỗi, offline, body đứt, Range bị bỏ qua, range sai, body nén quá lớn |
| Storage | JSON cũ/hỏng/cắt dở, journal dòng cuối thiếu, không có quyền ghi, ổ đầy, lần save bị gián đoạn |
| OS | Windows 10/11, Ubuntu 24.04, Fedora 44, Arch, openSUSE Tumbleweed và macOS; terminal/display session khác nhau; desktop output + tai nghe/Bluetooth hoặc không có audio output |
| Cài đặt | Windows installer/portable; Linux curl, archive ghi được, `.deb` do root quản lý; user thường/root; runtime đã có/thiếu; HOME có dấu cách; immutable host; x64/glibc hợp lệ và ARM/musl không hỗ trợ |
| Session | Guest, signed in, expired, logout giữa request, retry cooldown, chỉnh clock lùi/tiến |

Kích thước nhỏ ở trên là ca robustness; yêu cầu sản phẩm chưa chốt minimum
terminal cho mọi thao tác. Không tự coi việc thiếu các nút ở 1×1 là bug.

## Ma trận và cách tăng khả năng bắt lỗi

Với ca chọn bài rồi seek ngay, kết hợp các trục: trạng thái trước click (Idle/Buffering/Playing),
duration (0/1 giây/240 giây/không biết), repeat (Off/All/One), vị trí queue
(đầu/cuối), thời điểm response (trước/sau click) và thao tác tiếp theo
(không có/Next/Stop/seek lại/Pause). Các tổ hợp P0/P1 được ưu tiên theo nguy cơ:

- URL chưa trả về + click 100% + snapshot vẫn là Idle.
- URL chưa trả về + click 100% + click 0%/50% trước response.
- Click 100% + Next/Stop + URL/recovery/lyrics cũ đến sau.
- Click 100% + Buffering → Idle trước một frame Playing.
- EOF + Repeat All/One + hai mươi lần poll cùng trạng thái Idle.
- Clear hết upcoming + continuation đã gửi nhưng chưa trả về.
- Output đổi giữa seek/recovery + mất mạng + giữ đúng pause và thời gian bài.

Các test tự động mới gồm 19 hàm, với vòng lặp nhiều trạng thái/duration/pointer,
repeat/queue position và **16.384 bước queue** trên bốn seed cố định. Các bước
queue kiểm tra ID bài đang phát, history, duplicate và giới hạn bộ nhớ sau mỗi
thao tác. Shuffle có thứ tự riêng theo runtime; assertions kiểm tra invariants,
không yêu cầu một thứ tự shuffle cụ thể.

Những tổ hợp cần audio, máy Windows hoặc provider thật vẫn phải chạy tích hợp.
Không suy diễn một mô phỏng event đã pass thành một luồng âm thanh đã pass.

## Điều kiện đóng một đợt test

- Full và Clippy pass; Stress pass với số lần/thread được ghi lại.
- Với thay đổi installer/updater: ShellCheck, giả lập installer, smoke release trên
  bốn distro và checksum pass; ghi riêng ca desktop chưa chạy. Đối chiếu nhãn
  Settings: tray chỉ Windows, Updates Windows/Linux ghi được, còn lại cả hai.
- Mọi ca P0/P1 có kết quả Pass/Fail/Blocked/Not run, cùng bằng chứng và môi trường.
- Bug tái hiện được có regression test trước khi sửa; lưu kết quả trước/sau.
- Keyboard-only và mouse-only hoàn thành launch → search → play → seek → queue
  → background → restore → quit trên Windows thật.
- Bộ 100 bài và soak 8 giờ được ghi riêng: tỷ lệ start lần đầu/sau fallback,
  hạn chế account/region đã xác nhận, bộ nhớ steady state và mức tăng sau soak.
- Không ghi Pass cho nút không tồn tại hoặc tính năng chưa được triển khai;
  đối chiếu hành vi thực tế trước khi kết luận.

## Mẫu ghi kết quả / bug

```text
Case ID:
Build/commit + working tree:
OS / terminal / kích thước / output:
Guest hay account test:
Dữ liệu / duration / repeat / queue:
Steps và thời điểm (ms):
Expected:
Actual:
Pass / Fail / Blocked / Not run:
Tỷ lệ tái hiện (x/n), seed, thứ tự response:
Log / ảnh / recording đã bỏ thông tin session:
Regression test / issue:
```
