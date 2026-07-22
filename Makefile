install:
	cd ./rustVer; cargo build --release; cp ./target/release/compareDirs2 $$HOME/.local/bin

build:
	cd ./rustVer; cargo build --release

run:
	cd ./rustVer; cargo run

clean:
	rm -rf ./rustVer/target; rm -rf ./rustVer/compareDirs2; rm -rf ./rustVer/Cargo.lock;

