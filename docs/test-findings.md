# Kết quả bug hunt — 2026-10-10

Scope: MTUI 0.9.0 trên Windows, working tree hiện tại. Các snapshot/response trong
regression App được điều khiển bằng fixture; không suy diễn chúng thành kết quả
âm thanh/provider thật. [Catalog](test-cases.md) có **94 kịch bản**, trong đó 17
kịch bản có 19 hàm assertion mới; các ca tích hợp M/W chưa mặc định được đánh dấu Pass.

## Bug tái hiện và đã sửa

| Bug | Ca tái hiện / trước sửa | Thay đổi / regression |
| --- | --- | --- |
| MTUI-TEST-001 — mất seek lúc bắt đầu bài | Chọn A → click cuối trước resolve: chỉ có Load/Play, không Seek. Kéo 100%→0%→50% cũng mất vị trí cuối. Biến thể warm: resolve đã trả về nhưng snapshot còn Idle cũng bỏ click. | Giữ seek cuối trong lúc pending; gửi sau Play. Xóa khi đổi bài/Stop/failure. Sau resolve, không loại seek chỉ vì snapshot Idle chưa cập nhật. SEEK-01..04 và hai test timing đầu trong `app::scenarios`. |
| MTUI-TEST-002 — Clear không hủy top-up khi queue chỉ còn bài hiện tại | A là row duy nhất, page B đang chờ; Clear rồi page về: queue từ 1 thành 2 row. | Clear vẫn mint epoch mới nếu có continuation/top-up, dù removed=0. QUE-01; QUE-02/03 kiểm tra pause/Stop/repeat với page muộn. |
| MTUI-TEST-003 — EOF trong Buffering không advance | A → Buffering → click 100% → resolve → Idle, không có frame Playing; pending B vẫn None. | Nhận Buffering→Idle không lỗi là EOF, bên cạnh Playing→Idle; không advance khi snapshot hoặc App có playback error. SEEK-10/11; lỗi resolve được test riêng để không auto-skip nhầm. |
| MTUI-TEST-004 — EOF cũ xóa tracking bài mới | A đang phát → chọn B, B còn pending → EOF A đến. Pending vẫn B nhưng `listening` của B bị xóa, khiến tracking history/recovery không còn. | Chỉ finalize listening cùng nhánh advance khi không có bài mới pending. SEEK-08 kiểm tra cả pending lẫn listening identity của B. |

Bốn bug trên được tái hiện bằng assertion fail trước khi sửa. Đây là lỗi logic
trong các thứ tự sự kiện đã mô phỏng, không phải suy đoán từ log mạng.

## Bằng chứng trước sửa

Các log local dưới `target/test-runs/baseline/` (ignored bởi Git):

- `scenarios-before.log`: 12 test, 9 pass, 3 fail; fail ở seek đầu bài/scrub và Clear.
- `scenarios-intermediate.log`: hai lỗi đầu đã sửa; 13/14 pass, còn EOF Buffering fail.
- `warm-seek-before.log`: riêng biến thể URL đã trả về nhưng snapshot còn Idle fail.
- `old-eof-identity-before.log`: SEEK-08 fail vì listening identity của bài mới bị xóa.
- `tests.log`: lượt chạy sandbox ban đầu gặp access denied cho temp/socket
  (`os error 5/10013`). Đây là lỗi môi trường; chưa tính là bug sản phẩm.

## Validation sau sửa

| Kiểm tra | Kết quả | Bằng chứng |
| --- | --- | --- |
| Full workspace/all targets, 4 thread | **531 pass, 0 fail, 47 ignored** | `target/test-runs/20261010-214300-a7d9a4ae/summary.json` và `tests-1.log` |
| Clippy workspace/all targets, warnings là lỗi | **Pass** | Cùng lần Full, `clippy.log` |
| Smoke, PowerShell 7 | **167 pass, 0 fail, 6 ignored** | `target/test-runs/20261010-214514-71c16e53/summary.json` |
| Stress toàn bộ suite, 10 lần, 8 thread | **10/10 pass; 5.310 lượt test pass, 0 fail** | `target/test-runs/20261010-214407-d3c18738/summary.json`, `tests-1.log` đến `tests-10.log` |
| Hai probe AAC local | **2/2 pass**, qua PowerShell 7 | `target/test-runs/20261010-214409-3e1858d0/summary.json`, `audio-1.log`, `audio-2.log` |
| Full và Clippy khi chuẩn bị v0.9.1 | **531 pass, 0 fail, 47 ignored; Clippy Pass** | `target/test-runs/20261010-225232-bb0f5589/summary.json` |
| Windows GUI/tray/hotplug, live account, 100 bài, soak 8 giờ | **Not run** | Ca M/W trong catalog |

Full và Stress dùng profile/cache/tmp riêng. Live API, sign-in, history write và
các ignored preview không được bật bởi hai suite này. CI đã được cấu hình dùng
runner trên ba OS và giữ log/summary; chưa có kết quả CI remote của thay đổi này.

Probe decoder đối chiếu 2.048 sample sau resume ở giây thứ 8 với sample gốc.
Probe player dùng output volume 0 và HTTP loopback, xác nhận 403 đầu track được
recover, Stop chặn replacement muộn và 403 lặp không tạo retry vô hạn. Fixture
AAC có sẵn dài 524.288 bytes; hai probe không kiểm tra decode toàn bài hoặc
chuỗi click cuối bài bằng chuột trên console thật.

Runner cũng đã được kiểm tra với fixture bị thiếu: trả exit code 1 và summary
ghi đúng rằng không có ignored test nào đã chạy. Bộ lọc không chọn được test
thực thi bị coi là lỗi, tránh kết quả xanh từ một lượt chạy rỗng.

Lượt chuẩn bị release đầu tiên phát hiện assertion mini-player phụ thuộc thời
gian marquee (`target/test-runs/20261010-225047-7a6a4c6b/tests-1.log`). Fixture
được nới đủ chỗ cho cả title và artist, nên assertion không phụ thuộc thời điểm
chạy; hành vi render sản phẩm không đổi. Full và Clippy sau đó đều pass như bảng
trên. Riêng thay đổi title Windows được kiểm tra bằng hai lần khởi động console
helper: cả hai hiển thị `MTUI`, không hiển thị đường dẫn executable. Kiểm tra này
không thay cho các ca GUI/tray/hotplug còn lại.

## Việc còn cần kiểm chứng

- SEEK-13..20 cần audio/terminal/timing thật, gồm bài cực ngắn kết thúc giữa
  hai frame, seek khi đổi output và thứ tự A→B→A. Fix EOF ở trên xác nhận khi
  Buffering hoặc Playing đã được quan sát, không chứng minh mọi trạng thái
  trung gian bị bỏ lỡ giữa hai lần poll đều được xử lý.
- Probe AAC ngắn chỉ chứng minh các assertion trong probe; không thay cho
  decode cả track, nghe trực tiếp hay reliability trên provider.
- Chạy các ca account-write có chủ đích trên account/playlist test. Không coi
  callback giả lập đã pass là xác nhận thay đổi thật trên YouTube Music.
- Chốt minimum terminal size và đối chiếu feature gaps trong requirements;
  đo performance của toàn app riêng với helpers.

Không kết luận "hết bug" từ lượt chạy này. Kịch bản, regression, runner và báo
cáo là bộ nền để tiếp tục tìm lỗi và ngăn những lỗi đã tái hiện quay lại.
