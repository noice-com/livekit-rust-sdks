cd docker && docker build -t livekit-libwebrtc-builder .

cd ..

docker run --rm -it -e HOME=/tmp -v $(pwd):/workspace livekit-libwebrtc-builder /bin/bash -c "cd /workspace && rustup default stable && LK_CUSTOM_WEBRTC=/workspace/webrtc-sys/libwebrtc/linux-x64-release/ cargo build --release"
