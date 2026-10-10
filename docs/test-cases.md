# Kịch bản test MTUI, gồm các trường hợp cá biệt

Mỗi dòng là một kịch bản có thể chạy lại; các trạng thái và biến thể được ghi
trong setup/steps. Đọc [testing.md](testing.md) để chuẩn bị dữ liệu, chạy runner
và ghi kết quả. P0: crash, sai bài/tài khoản hoặc mất dữ liệu; P1: sai playback,
treo hoặc hỏng luồng chính; P2: layout/nội dung phụ; P3: trường hợp ít ảnh hưởng.

**A** = có assertion trong `app::scenarios`, chạy tự động trong Full/Smoke.
**M** = cần chạy tích hợp/thủ công; có thể đã có component test cho một phần,
nhưng chưa được coi là hoàn thành ca này. **W** = cần tài khoản/playlist test
và chủ đích ghi dữ liệu; không thuộc runner mặc định. Kết quả thực tế nằm trong
[test-findings.md](test-findings.md), không mặc định Pass cho các dòng M/W.

Mặc định dùng guest, bài 240 giây, queue [A,B,C,D], Repeat Off, volume thấp,
terminal 100×36. Với A, thời điểm worker được điều khiển bằng fixture; với M,
phải ghi rõ trạng thái nhìn thấy và thời điểm thao tác.

## Seek và kết thúc bài

| ID | Ưu tiên | Setup / thao tác | Kết quả mong đợi | Chạy |
| --- | --- | --- | --- | --- |
| SEEK-01 | P1 | Click A rồi click 100% trước URL; lặp với snapshot Idle/Buffering/Playing; URL về nhưng snapshot chưa đổi | Play A rồi áp dụng seek 240s; không seek nhầm bài cũ, không mất click ở warm/cold timing | A |
| SEEK-02 | P1 | A đang resolve; click 100% → 0% → 50%; URL trả về | Chỉ áp dụng vị trí cuối 120s sau Play | A |
| SEEK-03 | P0 | Chọn A → click 100% → chọn B; resolve A rồi B | Chỉ B được Play; seek A không truyền sang B | A |
| SEEK-04 | P1 | Chọn A → click 100% → Stop; URL A trả về | Im lặng; page/pending bị xóa; không phát lại | A |
| SEEK-05 | P1 | Duration không biết; click cuối khi loading/playing/paused | Không dựng seek 0 hoặc một duration giả | A |
| SEEK-06 | P1 | Duration 0/1ms/240s; pointer 0/1/9999/10000/65535; ba trạng thái active | Target nằm trong [0,duration]; không thay pause state | A |
| SEEK-07 | P1 | Click cuối khi Playing; đầu/cuối queue; Off/All/One; poll Idle 20 lần | Off next/dừng, All wrap, One replay; mỗi EOF chỉ một Resolve | A |
| SEEK-08 | P0 | A đang phát, chọn B; EOF của A xuất hiện khi B còn pending | B không bị bỏ qua; giữ tracking history/recovery B; chỉ một Resolve B | A |
| SEEK-09 | P0 | Chọn B; NeedsUrl(A,240s) tới muộn | Không Resume A hay thay pending B | A |
| SEEK-10 | P1 | Chọn A → Buffering → click cuối → resolve → Idle, không có frame Playing | Queue vẫn sang B đúng một lần | A |
| SEEK-11 | P1 | Buffering → Idle với lỗi network | Báo lỗi; không diễn giải thành EOF để tự bỏ A | A |
| SEEK-12 | P1 | Pause A ở 120s rồi click 100% | Seek được gửi, giữ Paused; chưa tự phát B | A |
| SEEK-13 | P1 | Drag 0→100%; thu nhỏ/cỡ lại terminal trong drag; release ngoài bar | Không panic; không kích hoạt nút/row bị kéo ngang; target trong bounds | M |
| SEEK-14 | P1 | Seek 99.9%/100% đồng thời đổi output; lặp khi pause | Clock/âm thanh/pause đồng nhất; không restart 0, không mở hai engine | M |
| SEEK-15 | P1 | Seek cuối/backward đúng lúc mất mạng hoặc URL 403 | Retry có giới hạn ở target mới nhất; Retry/Skip hữu dụng; không treo | M |
| SEEK-16 | P1 | Click cuối → nhấn rewind/forward liên tiếp; dùng chuột và phím luân phiên | Thao tác hợp lệ cuối thắng; không overflow, không skip hai bài | M |
| SEEK-17 | P1 | Bài AAC 1 giây; click cuối ngay sau chọn, lặp 100 lần | Không bỏ EOF do tốc độ quá nhanh; không vòng resolve vô hạn | M |
| SEEK-18 | P1 | Bài >1 giờ; seek xa ngoài ring buffer, tới cuối rồi về đầu | Clock, lyrics và sample nghe được đúng target; bộ nhớ bounded | M |
| SEEK-19 | P1 | Chọn A → B → A trong lúc resolve/recovery A cũ còn chờ | Response thuộc lần A cũ không ghi đè ý định/seek của lần A mới | M |
| SEEK-20 | P1 | Duration metadata lệch với decoder, fragmented MP4, mixed video/audio | Cuối thực được nhận đúng; lỗi seek không giả thành bài hết | M |

### Script tích hợp trọng tâm: vừa chọn bài rồi click cuối

1. Chuẩn bị queue A,B,C; A có AAC local hoặc bài public đã xác nhận playable.
2. Đặt Repeat Off. Chọn A, click ô cuối progress ngay khi title A xuất hiện.
3. Ghi thời điểm click và trạng thái trước click; chờ resolve/recovery hoàn tất.
4. Xác nhận B bắt đầu đúng một lần, title/audio/lyrics đều thuộc B, không pause
   giả và không trở lại A sau response muộn. Kiểm tra tới khi các request đã
   settle; nếu quá deadline thì ghi Fail/Blocked, không chờ vô hạn.
5. Lặp với queue chỉ A (phải dừng), All (quay đầu), One (lặp A), click 100% rồi
   0% (phải về đầu A), click 100% rồi Next, Stop và Pause.
6. Lặp với warm cache/cold resolve, network chậm, duration không biết và đổi output.

Test tự động xác nhận command/event; script này xác nhận âm thanh và timing thật.

## Queue và continuation

| ID | Ưu tiên | Setup / thao tác | Kết quả mong đợi | Chạy |
| --- | --- | --- | --- | --- |
| QUE-01 | P1 | Chỉ còn A, continuation đang chờ; Clear rồi response chứa B về | Queue vẫn chỉ A; response epoch cũ bị loại | A |
| QUE-02 | P1 | Pause A rồi continuation về; Stop rồi một page khác về | Không tự Play khi paused; không hồi sinh queue đã Stop | A |
| QUE-03 | P1 | Top-up đang chờ; bật Repeat All; response cũ về | Loop trên queue hiện có; không append radio cũ | A |
| QUE-04 | P0 | 4 seed ×4096 bước insert/add/remove/move/shuffle/clear/absorb/advance/trim | Giữ current/history đúng, không duplicate, queue ≤86, memory IDs ≤200 | A |
| QUE-05 | P1 | Một upcoming unavailable/private rồi đến bài playable | Bounded auto-skip; tiếp tục bài playable; thông báo có nghĩa | M |
| QUE-06 | P1 | Cả queue không resolve được; provider luôn lỗi | Dừng ở retry/skip budget; không chạy xuyên radio vô hạn | M |
| QUE-07 | P0 | Radio A đang top-up; chọn playlist B; page A về sau | B không chứa các bài/page/title của A | M |
| QUE-08 | P1 | Provider gửi duplicate trong page, page toàn bài cũ, lặp token | Không duplicate/loop; token vô ích bị bỏ; không growth | M |
| QUE-09 | P1 | Selected ở bài đầu/cuối; queue trim sau nhiều lần advance | Selection đúng vùng còn giữ; current marker/scroll/hit-map đúng | M |
| QUE-10 | P1 | Queue đầy; Play next từ Home/Related nhiều lần | Ý định người dùng được chèn đúng; recommendation tail nhường; vẫn bounded | M |
| QUE-11 | P1 | Prefetch B đã xong; xóa B ngay trước EOF A | B không phát từ stale prefetch; bài kế tiếp thật được chọn | M |
| QUE-12 | P1 | Reorder/shuffle upcoming trong lúc prefetch; spam Next/Previous | Queue và audio cùng ID; không mất/lặp bài ngoài repeat | M |
| QUE-13 | P1 | Clear đúng lúc EOF/auto-next/top-up cùng đến | Giữ current hợp lệ; không resurrect upcoming; không panic | M |
| QUE-14 | P1 | Playlist 10.000 bài, continuation cuối rỗng/lỗi | Chỉ giữ cửa sổ bounded, không mất tail trang; có thể thoát/retry | M |

## Response đến muộn và thao tác đồng thời

| ID | Ưu tiên | Setup / thao tác | Kết quả mong đợi | Chạy |
| --- | --- | --- | --- | --- |
| RACE-01 | P0 | Chọn A rồi B; cover/lyrics A về muộn | Nội dung B không bị A ghi đè | A |
| RACE-02 | P1 | Search A → filter → Search B; trả B trước A | Giữ kết quả/filter/selection B; response A không đổi busy/status/menu | M |
| RACE-03 | P1 | Album A đang tải; Back/Home rồi mở Artist B; response A về | Trang B và history/scroll không bị overwrite | M |
| RACE-04 | P1 | Refresh Home liên tiếp, quay lại sau load | Generation mới thắng; heading/order/scroll hợp lệ | M |
| RACE-05 | P1 | Double click/Enter bài 20 lần trong 1 giây | Không nhiều player/helper tồn tại, không request backlog tăng vô hạn | M |
| RACE-06 | P1 | Menu theo row đang mở; search/browse response thay row | Menu đóng hoặc vẫn thuộc đúng row; Enter không hành động lên row mới ngoài ý muốn | M |
| RACE-07 | P1 | Overlay Share/Save/Settings mở; click vào Home/row/player bên dưới | Modal sở hữu input; không click xuyên; Escape đóng đúng một lớp | M |
| RACE-08 | P1 | Source/player worker disconnect trong khi pending | UI còn dùng được; pending không treo vô hạn; lỗi/retry hợp lệ | M |
| RACE-09 | P1 | Related/Lyrics/Comments đổi tab nhanh khi đổi bài | Bounded cancellation; không fetch trùng vô ích; panel đúng ID | M |
| RACE-10 | P0 | Đóng cửa sổ/Quit trong lúc save settings/helper/resolver hoạt động | Flush dữ liệu hợp lệ; quit hoàn tất; không helper/player bị bỏ lại | M |

## Network, resolver và audio

| ID | Ưu tiên | Setup / thao tác | Kết quả mong đợi | Chạy |
| --- | --- | --- | --- | --- |
| NET-01 | P1 | URL đầu 403 rồi URL mới 206; lỗi trước sample đầu | Recover một cách bounded; bắt đầu đúng bài, không cần restart app | M |
| NET-02 | P1 | 403 giữa track, 403 ở range cuối; lặp khi seek | Giữa bài resume đúng position; EOF thật không replay đoạn cuối | M |
| NET-03 | P1 | Server bỏ Range và trả full body; range tiếp theo | Không splice/duplicate bytes, audio không nhảy lùi | M |
| NET-04 | P1 | Content-Range sai start/total, 416, Content-Length lệch | Từ chối dữ liệu sai; recovery có giới hạn, báo nguyên nhân | M |
| NET-05 | P1 | Body ngắt giữa chunk rồi reconnect đúng range | Không lặp/mất bytes; retry đúng offset; không đọc mãi | M |
| NET-06 | P1 | Network offline 5s/30s, DNS/TLS lỗi, chuyển Wi-Fi | Input còn phản hồi; recovery/Retry/Skip không treo | M |
| NET-07 | P1 | 429/500/503 lặp lại | Backoff/budget; không biến permission/rate limit thành sign-in popup | M |
| NET-08 | P0 | Metadata/JPEG quá lớn, gzip giải nén lớn, ảnh kích thước bất thường | Chặn theo budget decoded bytes/dimensions trước cấp phát lớn | M |
| NET-09 | P1 | yt-dlp/runtime/provider thiếu, file bị cắt dở, process lỗi | Lỗi setup/repair rõ; không để file cài dở được xem là executable hợp lệ | M |
| NET-10 | P1 | Đổi codec/itag/CDN khi resume; AAC trong mixed MP4 | Chọn audio track; không dùng byte offset của representation khác | M |

## Account, session và thao tác có ghi dữ liệu

| ID | Ưu tiên | Setup / thao tác | Kết quả mong đợi | Chạy |
| --- | --- | --- | --- | --- |
| AUTH-01 | P1 | Guest first-run → search → play → Home | Luồng guest dùng được; account actions giải thích cần sign-in | M |
| AUTH-02 | P1 | Sign-in thành công/hủy/đóng helper, Google đòi verification | UI trạng thái đúng, helper đóng khi xong; cancel không freeze | M |
| AUTH-03 | P0 | Session expiry/renewal khi đang nghe; 401 rồi success | Giữ bài/position; chỉ auth failure thật thúc renewal; cooldown không loop | M |
| AUTH-04 | P0 | Logout khi Rating/Playlists đang chờ; đổi session trước response | Không để response/action cũ theo sang tài khoản mới; clear state đúng | M |
| AUTH-05 | P1 | Like/Unlike click liên tục; timeout sau write; response sai video | Pending chặn duplicate; chỉ xác nhận server mới đổi state; không retry write mù | W |
| AUTH-06 | P0 | Mở Save cho A, playback sang B, chọn playlist | Lưu A; không lưu B; đúng playlist; không duplicate bài đã có | W |
| AUTH-07 | P1 | Playlist readonly/xóa trước Save; acknowledgement mơ hồ | Báo failure đúng; verify exact song khi cần; không báo success giả | W |
| AUTH-08 | P0 | Logout giữa play/history retry; restart sau pending outbox | Không gửi history tài khoản cũ sang tài khoản mới; không mất/outbox duplicate | W |

## Search, Home, Artist, Album, Playlist và điều hướng

| ID | Ưu tiên | Setup / thao tác | Kết quả mong đợi | Chạy |
| --- | --- | --- | --- | --- |
| BRW-01 | P1 | Search rỗng, toàn spaces, tiếng Việt/CJK/emoji, query rất dài | Không panic; query hợp lệ hiển thị rõ; không loop request | M |
| BRW-02 | P1 | Filter All/Songs/Videos/Artists/Albums/Playlists; kết quả mixed/missing category | Đúng kind; song play, artist/album/playlist browse; thiếu dữ liệu không giả thành song | M |
| BRW-03 | P1 | Paste Music/YouTube/short URL, playlist params, malformed URL | Nhận diện ID hợp lệ; không chọn nhầm playlist/song; invalid input rõ | M |
| BRW-04 | P2 | Home shelf rỗng/partial/missing art; continuation lỗi | Header/order hợp lệ; loading/empty/error hiện đúng vùng | M |
| BRW-05 | P1 | Artist không top songs/không shelves/thiếu description | Mỗi section cursor đúng; play/open đúng type; layout không panic | M |
| BRW-06 | P1 | Artist → album → song → Now Playing → Back; đi >12 trang | Khôi phục route/selection/scroll; history bounded; không nested player loop | M |
| BRW-07 | P1 | Playlist rỗng/unavailable/private; Retry/Play/Shuffle | Không Play row ngoài bounds; retry được; actionable error | M |
| BRW-08 | P1 | Đối chiếu Library/history/favorites/import-export với requirements | Ghi feature gap nếu thiếu; không coi nút/menu placeholder là chức năng pass | M |

## Giao diện và input cá biệt

| ID | Ưu tiên | Setup / thao tác | Kết quả mong đợi | Chạy |
| --- | --- | --- | --- | --- |
| UI-01 | P1 | Resize 100×36 →1×1→48×18→216×50 trên mọi page/modal | Không panic; trở về kích thước thường vẫn thao tác được; hit-map không stale | M |
| UI-02 | P2 | Title/artist dài với e+combining accent, ZWJ emoji/skin tone/flag/RTL/CJK | Không split grapheme, không tràn column, identity còn đọc được | M |
| UI-03 | P1 | Focus search rồi Space/n/p/q/m/S/Ctrl-P, paste multiline, AltGr/surrogate | Input vào đúng nơi; không playback/quit ngoài ý muốn; Unicode hợp lệ | M |
| UI-04 | P1 | Right-click row khác selection; menu → Escape → Enter | Context đúng row clicked, Escape không kích hoạt row dưới | M |
| UI-05 | P1 | Wheel/drag trên blank spacer, heading, scrollbar, clock ngoài seek | Không play heading/blank; progress hit target kết thúc trước clock | M |
| UI-06 | P2 | Lyrics thiếu/plain/synced, offset trùng/out of order, scroll rồi đổi bài | Highlight/follow đúng; manual scroll không bị giật lại; tab state không lẫn bài | M |
| UI-07 | P2 | Kitty/Sixel/blocks/ASCII; mất cell-size/partial probe, cover đổi khi resize | Fallback đọc được, không image cũ che text; protocol không lỗi | M |
| UI-08 | P2 | Cover gần đen/đỏ/bão hòa; mono terminal; đổi theme khi modal mở | Text và selected item đọc được; palette bounded; không crash | M |

## Storage và khôi phục

| ID | Ưu tiên | Setup / thao tác | Kết quả mong đợi | Chạy |
| --- | --- | --- | --- | --- |
| STORE-01 | P1 | settings.json cũ/rỗng/hỏng; volume âm/NaN/∞/quá max | Migrate/default/clamp; không crash hoặc gain bất thường | M |
| STORE-02 | P0 | Save config liên tục trong lúc reader đọc; ngắt giữa replace | Reader thấy JSON hoàn chỉnh; giữ bản cũ nếu save thất bại | M |
| STORE-03 | P0 | Journal/outbox dòng cuối thiếu, entry cũ, duplicate nonce; restart | Giữ các record hợp lệ; ack đúng nonce; không replay write đã ack | M |
| STORE-04 | P1 | Không có quyền ghi/ổ đầy/đường dẫn Unicode dài | Lỗi rõ và recoverable; không freeze hoặc phá file tốt | M |
| STORE-05 | P0 | Kill app đang play/save; restart với queue/cache hỏng | Không crash; dữ liệu cấu trúc hợp lệ; kiểm tra gap persistence queue riêng | M |
| STORE-06 | P0 | Error có URL/cookie/Authorization; log đầy rồi rotate | Không lộ credentials; một backup bounded; report sanitized | M |

## Windows, thiết bị và tích hợp

| ID | Ưu tiên | Setup / thao tác | Kết quả mong đợi | Chạy |
| --- | --- | --- | --- | --- |
| OS-01 | P1 | Chưa có output, output đã lưu bị unplug, output switch thất bại | Báo lỗi/fallback đúng; thiết bị cũ chỉ bỏ khi mới mở được | M |
| OS-02 | P1 | Bluetooth/hotplug đổi default khi play/pause/seek/recovery | Track/clock/pause giữ đúng; không output reset về 0 | M |
| OS-03 | P1 | Đóng console → tray → relaunch 10 lần, thao tác transport trong tray | Một owner/player; relaunch show phiên đang chạy; playback liên tục | M |
| OS-04 | P1 | Sleep/wake/lock/unlock lúc seek, sign-in hoặc top-up | Không stale session loop; app còn dùng được; input/audio được phục hồi | M |
| OS-05 | P2 | Discord tắt/bật/restart, tên quá dài, seek/pause liên tục | Presence optional, bounded update; không chặn playback hay CPU tăng | M |
| OS-06 | P1 | Install/upgrade/uninstall non-admin, portable/relaunch, media keys | Đúng data paths/session retention/single instance; feature gap ghi rõ | M |

## Resource và reliability

| ID | Ưu tiên | Setup / thao tác | Kết quả mong đợi | Chạy |
| --- | --- | --- | --- | --- |
| SOAK-01 | P1 | 100 bài cố định đa dạng length/region/format; guest/account test | Đo first-attempt/fallback success theo requirements; phân loại restrictions | M |
| SOAK-02 | P1 | 8 giờ play radio, mở nhiều artwork/tab/page; mẫu mỗi phút | Sau warmup 40–60 MiB, growth ≤8 MiB và <60 MiB; CPU trung bình <2% trên máy đại diện | M |
| SOAK-03 | P1 | 1.000 thao tác search/Back/Next/seek/resize trong mạng chậm | Input không treo, thread/process/cache/backlog bounded | M |
| SOAK-04 | P2 | 20 warm launches; đo paint/input và helper tồn tại | TUI ≤1s, input ≤2s; đo helpers riêng, đóng khi xong | M |

## Thứ tự thực hiện

1. Chạy Smoke để bắt sai state/command nhanh; ưu tiên SEEK-01..12 và QUE-01..04.
2. Chạy Full; lưu các assertion fail trước sửa để phân biệt bug với sandbox.
3. Chạy Stress với ít nhất hai mức thread nếu có dấu hiệu lỗi phụ thuộc scheduling.
4. Chạy M theo P0 → P1 → P2; giữ cùng fixture và chỉ đổi một yếu tố khi tái hiện.
5. Chạy W có chủ đích trên tài khoản test; xác nhận trên server, dọn dữ liệu test.
6. Reliability/soak là gate riêng, không thay bằng một ảnh Task Manager hoặc
   số unit test pass. Ghi Not run cho gate chưa thực hiện.
