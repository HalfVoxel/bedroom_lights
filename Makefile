release:
	cargo build --release

flash: release
	cargo espflash flash --release --monitor -T ./partitions.csv --baud 921600