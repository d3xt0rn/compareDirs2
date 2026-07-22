install:
	cd ./rustVer; cargo build --release; cp ./target/release/compareDirs2 $$HOME/.local/bin

build:
	cd ./rustVer; cargo build --release

run:
	cd ./rustVer; cargo run

clean:
	rm -rf ./rustVer/target; rm -rf ./rustVer/compareDirs2; rm -rf ./rustVer/Cargo.lock;

help:
	@echo "You can run this commands"
	@echo "make build | build project"
	@echo "make install | build and install to /home/$$USER/.local/bin"
	@echo "make run | to run cmd. But i think it can have bugs"
	@echo "make clean | to clean build dir"
