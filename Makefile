image = linkshrink

# Get a list of all targets
targets = $(shell grep '^\.PHONY: .*$$' Makefile | sed 's/\.PHONY: //')
.DEFAULT_GOAL := help

.PHONY: help
help:
	@echo "Available targets:"
	@$(foreach target,$(targets),echo "  $(target)";)

.PHONY: clean
clean:
	cargo clean

.PHONY: docker-build
docker-build:
	cargo clean
	docker build -t $(image):latest .

.PHONY: docker-run
docker-run:
	docker-compose up -d

.PHONY: docker-stop
docker-stop:
	docker-compose stop