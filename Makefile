# SPDX-License-Identifier: AGPL-3.0-or-later

.PHONY: keys production-keys production-sysext-cert playground-keys build debug fleet playground extensions watchtower-extension chrome-extension ace-package aide-extensions fleet-extensions playground-extensions package fleet-package playground-package sign fleet-sign playground-sign verify pipeline fleet-pipeline playground-pipeline publish-r2 prune-r2 fleet-install-watchtower publish-fleet publish-playground installer fleet-installer playground-installer installer-preview clean clean-tools

keys:
	./scripts/provision-keys

production-keys:
	./scripts/provision-production-keys

production-sysext-cert:
	./scripts/reissue-production-sysext-certificate

playground-keys:
	./scripts/provision-playground-keys

build:
	./scripts/build

debug:
	./scripts/build --debug

fleet:
	./scripts/build --fleet

playground:
	./scripts/build --playground

extensions:
	./extensions/rat-game-16/build

watchtower-extension:
	./extensions/watchtower/build

chrome-extension:
	./extensions/chrome/build

ace-package:
	./scripts/package-ace

aide-extensions: watchtower-extension chrome-extension ace-package

fleet-extensions:
	./extensions/rat-game-16/build --fleet

playground-extensions:
	./extensions/rat-game-16/build --playground

package:
	./scripts/package

fleet-package:
	./scripts/package --fleet

playground-package:
	./scripts/package --playground

sign:
	./scripts/sign

fleet-sign:
	./scripts/sign --fleet

playground-sign:
	./scripts/sign --playground

verify:
	./scripts/verify

pipeline: build extensions package sign verify

fleet-pipeline:
	./scripts/fleet-pipeline

playground-pipeline:
	./scripts/playground-pipeline

publish-fleet:
	CARBIDEOS_R2_PREFIX=carbideos/fleet \
	./scripts/publish-r2 dist/update-feed/fleet

publish-playground:
	CARBIDEOS_R2_PREFIX=carbideos/playground \
	./scripts/publish-r2 dist/update-feed/playground

publish-r2:
	@test -n "$(SOURCE)" || { echo 'Usage: make publish-r2 SOURCE=dist/update-feed' >&2; exit 2; }
	./scripts/publish-r2 "$(SOURCE)"

prune-r2:
	@test -n "$(SOURCE)" || { echo 'Usage: make prune-r2 SOURCE=dist/update-feed [APPLY=1]' >&2; exit 2; }
	./scripts/prune-r2 "$(SOURCE)" $(if $(APPLY),--apply,)

# Usage: make fleet-install-watchtower <ip> [RELEASE=v0.3.6]
# Words after the target are the address, so they are swallowed as no-op goals.
# Released sysexts are fleet-signed, so this only suits fleet nodes.
ifeq ($(firstword $(MAKECMDGOALS)),fleet-install-watchtower)
install_watchtower_host := $(wordlist 2,$(words $(MAKECMDGOALS)),$(MAKECMDGOALS))
$(eval $(install_watchtower_host):;@:)
endif

fleet-install-watchtower:
	@test -n "$(install_watchtower_host)" || { echo 'Usage: make $@ <ip> [RELEASE=<tag>]' >&2; exit 2; }
	./scripts/carbide-install-watchtower "$(install_watchtower_host)" $(if $(RELEASE),--release $(RELEASE),)

# IMAGE= points at an arbitrary image, VERSION= at a packaged release under
# dist/. With neither, an image is built only if one is not already present.
installer_selection = $(if $(IMAGE),--image $(IMAGE),)$(if $(VERSION), --version $(VERSION),)

installer:
	./installer/build $(installer_selection)

fleet-installer:
	./installer/build --fleet $(installer_selection)

playground-installer:
	./installer/build --playground $(installer_selection)

# Renders every installer screen to installer/preview/*.png on the host.
installer-preview:
	cd installer && cargo run --features preview --bin carbide-preview

clean:
	sudo find mkosi.output -maxdepth 1 \( -type f -o -type l \) -name 'carbideos*' -delete 2>/dev/null || true
	rm -rf dist

clean-tools:
	sudo mkosi -f clean
