HOW TO USE AND BUILD:

In the streamcapture folder:
Run sender (only linux pipewire as of now) = 
1) cargo run --release (accepts all connections to port 4433)
2) cargo run --release <listen_addr> (accepts connections to listen addr to port 4433)

Run receiver = cargo run --release -p receiver --features="desktop" (IP or DDNS to whihch you wanna connect with port 4433. e.g.) 127.0.0.1:4433

In the receiver folder:
Build android app: 
1) cargo ndk -t arm64-v8a -P 26 -o ../android-app/app/src/main/jniLibs build --release
2) In the folder streamcapture/android-app = ./gradlew assembleRelease.
3) The apk file should be in app/build/outputs/apk/release
