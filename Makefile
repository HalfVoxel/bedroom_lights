release:
	cargo build --release --features log/max_level_error

flash:
	cargo espflash flash --release --monitor -T ./partitions.csv --baud 921600 --erase-parts app1 --target-app-partition app0
ota:
	cargo espflash save-image --chip esp32 --flash-size 4mb --partition-table ./partitions.csv --release target/xtensa-esp32-espidf/release/firmware.bin --features log/max_level_info
	cd ../ota_flasher && make uploader
	RUST_LOG=info ../ota_flasher/target/release/uploader --device "bedroom_lights 24:dc:c3:99:ea:39" --image target/xtensa-esp32-espidf/release/firmware.bin --version-file target/xtensa-esp32-espidf/release/build_id
