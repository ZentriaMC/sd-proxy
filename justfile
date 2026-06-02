build:
    docker build --platform linux/amd64 -t localhost/sd-proxy:latest .

# Test backend for sd-proxy's data path (reports the PROXY-protocol source back to the client).
build-echo:
    docker build --platform linux/amd64 --target proxy-echo -t localhost/proxy-echo:latest .
