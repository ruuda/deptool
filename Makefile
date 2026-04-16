# Static release binary for deployment to target hosts.
release:
	LIBZ_SYS_STATIC=1 cargo build --target x86_64-unknown-linux-musl --release
