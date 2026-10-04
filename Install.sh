#!/usr/bin/env bash
set -e # Dừng ngay lập tức nếu có bất kỳ lệnh nào bị lỗi

REPO_URL="https://github.com/tran-minhta/memoryclip.git"
TARGET_DIR="$HOME/memoryclip"
BINARY_NAME="memoryclip" # Tên ứng dụng khai báo trong Cargo.toml ([package] name)

echo "==> 1. Kiểm tra Git & C Toolchain (Linker)..."
if ! command -v git &> /dev/null || ! command -v gcc &> /dev/null; then
    echo "Đang cài đặt Git và Build Tools..."
    if command -v apt-get &> /dev/null; then
        sudo apt-get update && sudo apt-get install -y git build-essential pkg-config libssl-dev
    elif command -v pacman &> /dev/null; then
        sudo pacman -Sy --noconfirm git base-devel
    fi
fi

echo "==> 2. Kiểm tra & Cài đặt Rust / Cargo..."
if ! command -v cargo &> /dev/null; then
    echo "Cargo chưa có sẵn. Đang tự động cài đặt Rustup..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
    source "$HOME/.cargo/env" 2>/dev/null || export PATH="$HOME/.cargo/bin:$PATH"
fi

echo "==> 3. Kéo mã nguồn mới nhất..."
if [ -d "$TARGET_DIR/.git" ]; then
    echo "Thư mục đã tồn tại, đang đồng bộ với origin/main..."
    cd "$TARGET_DIR"
    git fetch origin
    git reset --hard origin/main
else
    echo "Đang clone repository..."
    git clone "$REPO_URL" "$TARGET_DIR"
    cd "$TARGET_DIR"
fi

echo "==> 4. Biên dịch dự án (Cargo Build Release)..."
cargo build --release

echo "==> 5. Cập nhật Binary vào /usr/local/bin/..."
if [ -f "target/release/$BINARY_NAME" ]; then
    sudo cp "target/release/$BINARY_NAME" /usr/local/bin/
    sudo chmod +x "/usr/local/bin/$BINARY_NAME"
    echo "==> Hoàn tất! Bạn có thể gõ '$BINARY_NAME' ở bất kỳ đâu để chạy."
else
    echo "Lỗi: Không tìm thấy file target/release/$BINARY_NAME"
    exit 1
fi
