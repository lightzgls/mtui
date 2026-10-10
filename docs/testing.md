# Kiểm thử MTUI

Mục tiêu là tìm lỗi qua **chuỗi thao tác và thứ tự sự kiện**, bên cạnh kiểm tra
từng chức năng riêng lẻ. Không coi số test pass là bằng chứng đã tìm hết bug.
Danh sách ca cụ thể nằm trong [test-cases.md](test-cases.md); kết quả đã quan sát
nằm trong [test-findings.md](test-findings.md).

## Chạy bộ kiểm tra

Chạy từ thư mục dự án bằng PowerShell 5.1 hoặc PowerShell 7:

```powershell
# Luồng thao tác, seek/recovery, session và thành phần giao diện
./scripts/test.ps1 -Suite Smoke

# Toàn workspace, tất cả target, rồi Clippy
./scripts/test.ps1 -Suite Full

# Chạy toàn bộ test nhiều lần để tìm lỗi phụ thuộc timing/thread
./scripts/test.ps1 -Suite Stress -Iterations 10 -Threads 8

# Hai probe AAC trên localhost: decoder và recovery/Stop, volume 0
./scripts/test.ps1 -Suite Audio -AudioFixture ./target/playback-regression.m4a
```

Mặc định dùng dependency đã có trên máy và Cargo.lock. Nếu máy mới chưa tải
dependency, thêm `-Online`. Cờ này cho phép Cargo tải dependency; **không bật
các test `#[ignore]`**. CI chạy Full trên Windows, Linux và macOS.
Suite Audio chỉ chạy đúng hai test AAC local được chọn sẵn. Nó cần file fixture
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
   resize lúc drag và nghe để đối chiếu clock/lyrics. Chạy theo ca trong catalog,
   ghi thiết bị, terminal, thời gian và kết quả riêng.
4. **Provider/account trực tiếp:** kiểm tra nội dung/search/account thật khi có
   điều kiện. Dùng tài khoản test và playlist test cho ca ghi dữ liệu. Các test
   history có thể ghi lên tài khoản đang đăng nhập, nên không chạy cả danh sách
   ignored bằng một lệnh.

Các test ignored được giữ nguyên trừ hai regression UI/actions đã được đổi sang
fixture thuần offline. Preview, live API, audio thật và history write trong
inventory **không có nghĩa đã pass**.

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
| OS | Windows 10/11, desktop output + tai nghe/Bluetooth; console/terminal khác nhau; không có audio output |
| Session | Guest, signed in, expired, logout giữa request, retry cooldown, chỉnh clock lùi/tiến |

Kích thước nhỏ ở trên là ca robustness; yêu cầu sản phẩm chưa chốt minimum
terminal cho mọi thao tác. Không tự coi việc thiếu các nút ở 1×1 là bug.

## Ma trận và cách tăng khả năng bắt lỗi

Với SEEK-01, kết hợp các trục: trạng thái trước click (Idle/Buffering/Playing),
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
- Mọi ca P0/P1 có kết quả Pass/Fail/Blocked/Not run, cùng bằng chứng và môi trường.
- Bug tái hiện được có regression test trước khi sửa; lưu kết quả trước/sau.
- Keyboard-only và mouse-only hoàn thành launch → search → play → seek → queue
  → background → restore → quit trên Windows thật.
- Bộ 100 bài và soak 8 giờ được ghi riêng. Theo requirements: >=98/100 start
  lần đầu, >=99/100 sau fallback, tách riêng hạn chế account/region đã xác nhận;
  steady state 40–60 MiB, tăng không quá 8 MiB sau soak, vẫn dưới 60 MiB.
- Đánh dấu thiếu tính năng theo requirements là gap; không ghi Pass cho nút
  không tồn tại. Library đầy đủ, import/export, media-key và persistence queue
  cần đối chiếu lại implementation trước khi kết luận.

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
