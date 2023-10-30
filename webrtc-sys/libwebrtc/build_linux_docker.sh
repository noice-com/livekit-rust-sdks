docker build -t livekit-libwebrtc-builder .

docker run --rm -it -e HOME=/tmp -v $(pwd):/workspace livekit-libwebrtc-builder /bin/bash -c "cd /workspace && ./build_linux.sh --arch x64"
