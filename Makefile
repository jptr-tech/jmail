APP_ID     ?= tech.jptr.jmail
MANIFEST   ?= tech.jptr.jmail.yaml
BUILD_DIR  ?= build-dir
STATE_DIR  ?= .flatpak-builder
CARGO      ?= cargo

.PHONY: help flatpak run test mock clean

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | \
		awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-10s\033[0m %s\n", $$1, $$2}'

flatpak: ## Build and install the Flatpak
	flatpak-builder --user --install --force-clean --assumeyes \
		--disable-rofiles-fuse --state-dir=$(STATE_DIR) $(BUILD_DIR) $(MANIFEST)

run: ## Run the app locally with cargo
	$(CARGO) run -p jmail-ui

test: ## Run the core test suite
	$(CARGO) test -p jmail-core

mock: ## Run the mock IMAP server
	$(CARGO) run -p jmail-mock-imap

clean: ## Remove Flatpak build and state directories
	rm -rf $(BUILD_DIR) $(STATE_DIR)
