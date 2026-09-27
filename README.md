# memoryclip

Tự động theo dõi clipboard, lưu mỗi nội dung thành file kèm thời gian, ghi chú và
nhãn, rồi index bằng SQLite FTS5 để tìm lại sau này.

Chạy được trên **Linux (X11/Wayland), macOS, Windows**. Chỉ có CLI — không server,
không web UI, không mở cổng mạng.

## Cài đặt

```sh
cargo build --release
cp target/release/memoryclip ~/.local/bin/
```

## Dùng

```sh
memoryclip run                     # chạy daemon theo dõi clipboard
memoryclip search "đăng nhập"      # tìm kiếm full-text
memoryclip recent                  # danh sách mới nhất
memoryclip get 42                  # xem toàn văn
memoryclip copy 42                 # copy về clipboard
memoryclip note 42 "ghi chú của tôi"
memoryclip tag 42 công-viec        # gắn nhãn
memoryclip pin 42                  # ghim, không bao giờ bị prune
memoryclip rm 42
memoryclip stats
memoryclip prune --dry-run         # xem trước sẽ dọn gì
```

Chạy nền khi đăng nhập:

```sh
memoryclip install-service --enable
```

Lệnh này sinh `~/.config/systemd/user/memoryclip.service` rồi chạy
`systemctl --user enable --now memoryclip`.

## Tìm kiếm tiếng Việt

Index dùng **FTS5 tokenizer `trigram`**, không phải `unicode61`.

Lý do: tiếng Việt không tách từ bằng dấu cách — `đăng nhập` là **một** từ. Với
`unicode61`, bỏ dấu rồi tìm tiền tố sẽ hỏng:

| Truy vấn | `unicode61 remove_diacritics 2` | `trigram` |
|---|---|---|
| `đăng nhập` | khớp | khớp |
| `khau*` | **không khớp** | — |
| `khẩu` (chuỗi con) | — | **khớp** |

`trigram` khớp được cả từ nguyên vẹn lẫn chuỗi con nằm giữa từ, không cần bộ tách
từ tiếng Việt. Đánh đổi: truy vấn phải dài **ít nhất 3 ký tự**, ngắn hơn sẽ tự
chuyển sang quét `LIKE`.

## Chỉ lưu text

| Nội dung clipboard | Kết quả |
|---|---|
| Text thuần | lưu nguyên văn |
| Copy file (X11/Wayland) | lưu danh sách tên file |
| Ảnh | bỏ qua |
| HTML, RTF, định dạng tuỳ biến | bỏ qua |

Đa số ứng dụng khi copy văn bản từ trang web đã đặt sẵn bản `text/plain` trong
clipboard, nên vẫn lưu được mà không cần đọc HTML.

> **Hạn chế đã biết (Windows):** copy file trong Explorer dùng định dạng
> `CF_HDROP`, chưa được hỗ trợ. Cần đọc `windows-sys` để lấy danh sách file.

## Lưu trữ

```
~/.local/share/memoryclip/          # %APPDATA%\ hoặc ~/Library/Application Support/
├── memoryclip.db                   # SQLite: index + FTS5
├── clips/2026/09/26/<hash>.txt     # nội dung thuần, địa chỉ theo hash
├── vault/<hash>.enc                # nội dung nhạy cảm đã mã hoá
├── master.key                      # chỉ khi không có OS keychain
└── config.toml
```

File được đặt tên theo BLAKE3 của nội dung, nên nội dung trùng nhau chỉ lưu một
bản.

## Bảo mật

Một **master key duy nhất cho mỗi máy**, sinh lần đầu chạy và lưu trong OS
credential store (Windows Credential Manager, macOS Keychain, Linux Secret
Service). Không có credential store thì rơi về file `0600` kèm cảnh báo.

Nội dung nhạy cảm được mã hoá bằng **XChaCha20-Poly1305**, mỗi clip một subkey
riêng derive bằng HKDF từ master key. `hash` của clip được truyền vào làm
associated data, nên ciphertext không thể dán sang dòng khác.

**Quan trọng:** nội dung nhạy cảm *không* được đưa vào FTS index. Chỉ còn hash
và preview đã che, nên tìm kiếm không lộ plaintext — `memoryclip get` mới giải mã.

Các rule phát hiện: private key, JWT, AWS key, GitHub token, Slack token, Google
API key, Stripe key, OpenAI key, bearer token, `key: value` chung, chuỗi entropy
cao. Bật/tắt từng rule trong `config.toml`.

## Cấu hình

`config.toml` nằm trong thư mục dữ liệu. Xem `config.example.toml` để biết đủ
tuỳ chọn. Mặc định quan trọng:

```toml
[limits]
max_clip_bytes = 262144     # bỏ qua clip > 256 KB

[retention]
max_age_days = 90           # 0 = tắt
max_total_bytes = 524288000 # 0 = tắt

[secret]
enabled = true
rules = []                  # rỗng = bật tất cả
```

Clip đã `pin` không bao giờ bị prune.

## Ghi chú kỹ thuật

**Clipboard X11 là owner-based.** Selection thuộc về process đã set nó và biến mất
khi process thoát. Vì vậy `memoryclip copy` chuyển việc giữ clipboard cho `xclip`
hoặc `xsel` (chúng tự fork ra nền). Nếu không có công cụ nào, daemon giữ process
sống thêm vài trăm mili-giây. Trên Windows/macOS/Wayland, OS giữ dữ liệu nên
không cần.

**Vòng lặp tự ghi.** Khi daemon copy một clip, giá trị đó quay lại clipboard và
bị bắt lại. Hash của nội dung đã bắt được nằm trong bộ nhớ cache, nên lần đọc
lặp lại bị bỏ qua. Cache giới hạn 4096 mục để daemon chạy lâu không phình bộ
nhớ.

**Cơ chế phát hiện thay đổi** khác nhau theo OS vì API gốc khác nhau:

| OS | Cách phát hiện |
|---|---|
| Windows | `GetClipboardSequenceNumber` — chỉ đọc khi có thay đổi thật |
| macOS | `NSPasteboard.changeCount` |
| X11 / Wayland | Không có bộ đếm, phải poll + so hash |

## Phát triển

```sh
cargo test
cargo build --release
```

77 test bao phủ: tìm kiếm tiếng Việt, chống rò rỉ vault, escape SQL, round-trip
mã hoá, dedupe, prune.
