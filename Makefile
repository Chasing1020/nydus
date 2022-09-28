all: build

all-build: build contrib-build

all-release: release contrib-release

all-static-release: static-release docker-static contrib-release

all-install: install contrib-install

all-clean: clean contrib-clean

TEST_WORKDIR_PREFIX ?= "/tmp"
INSTALL_DIR_PREFIX ?= "/usr/local/bin"
DOCKER ?= "true"

CARGO ?= $(shell which cargo)
CARGO_BUILD_GEARS = -v ~/.ssh/id_rsa:/root/.ssh/id_rsa -v ~/.cargo/git:/root/.cargo/git -v ~/.cargo/registry:/root/.cargo/registry
SUDO = $(shell which sudo)
CARGO_COMMON ?= 

EXCLUDE_PACKAGES =
UNAME_M := $(shell uname -m)
UNAME_S := $(shell uname -s)
STATIC_TARGET = $(UNAME_M)-unknown-linux-musl
ifeq ($(UNAME_S),Linux)
	CARGO_COMMON += --features=virtiofs
ifeq ($(UNAME_M),ppc64le)
	STATIC_TARGET = powerpc64le-unknown-linux-gnu
endif
ifeq ($(UNAME_M),riscv64)
	STATIC_TARGET = riscv64gc-unknown-linux-gnu
endif
endif
ifeq ($(UNAME_S),Darwin)
	EXCLUDE_PACKAGES += --exclude nydus-blobfs
ifeq ($(UNAME_M),amd64)
	STATIC_TARGET = x86_64-apple-darwin
endif
ifeq ($(UNAME_M),arm64)
	STATIC_TARGET = aarch64-apple-darwin
endif
endif
RUST_TARGET_STATIC ?= $(STATIC_TARGET)

CTR-REMOTE_PATH = contrib/ctr-remote
DOCKER-GRAPHDRIVER_PATH = contrib/docker-nydus-graphdriver
NYDUSIFY_PATH = contrib/nydusify
NYDUS-OVERLAYFS_PATH = contrib/nydus-overlayfs

current_dir := $(shell dirname $(realpath $(firstword $(MAKEFILE_LIST))))
env_go_path := $(shell go env GOPATH 2> /dev/null)
go_path := $(if $(env_go_path),$(env_go_path),"$(HOME)/go")

# Set the env DIND_CACHE_DIR to specify a cache directory for
# docker-in-docker container, used to cache data for docker pull,
# then mitigate the impact of docker hub rate limit, for example:
# env DIND_CACHE_DIR=/path/to/host/var-lib-docker make docker-nydusify-smoke
dind_cache_mount := $(if $(DIND_CACHE_DIR),-v $(DIND_CACHE_DIR):/var/lib/docker,)

# Functions

# Func: build golang target in docker
# Args:
#   $(1): The path where go build a golang project
#   $(2): How to build the golang project
define build_golang
	echo "Building target $@ by invoking: $(2)"
	if [ $(DOCKER) = "true" ]; then \
		docker run --rm -v ${go_path}:/go -v ${current_dir}:/nydus-rs --workdir /nydus-rs/$(1) golang:1.18 $(2) ;\
	else \
		$(2) -C $(1); \
	fi
endef

.PHONY: .release_version .format .musl_target \
	all all-build all-release all-static-release build release static-release

.release_version:
	$(eval CARGO_BUILD_FLAGS += --release)

.format:
	${CARGO} fmt -- --check

.musl_target:
	$(eval CARGO_BUILD_FLAGS += --target ${RUST_TARGET_STATIC})

# Targets that are exposed to developers and users.
build: .format
	${CARGO} build $(CARGO_COMMON) $(CARGO_BUILD_FLAGS)
	# Cargo will skip checking if it is already checked
	${CARGO} clippy $(CARGO_COMMON) --workspace $(EXCLUDE_PACKAGES) --bins --tests -- -Dwarnings

release: .format .release_version build

static-release: .musl_target .format .release_version build

clean:
	${CARGO} clean

install: release
	@sudo mkdir -m 755 -p $(INSTALL_DIR_PREFIX)
	@sudo install -m 755 target/release/nydusd $(INSTALL_DIR_PREFIX)/nydusd
	@sudo install -m 755 target/release/nydus-image $(INSTALL_DIR_PREFIX)/nydus-image
	@sudo install -m 755 target/release/nydusctl $(INSTALL_DIR_PREFIX)/nydusctl

ut:
	TEST_WORKDIR_PREFIX=$(TEST_WORKDIR_PREFIX) RUST_BACKTRACE=1 ${CARGO} test --workspace $(EXCLUDE_PACKAGES) $(CARGO_COMMON) -- --skip integration --nocapture --test-threads=8

smoke: ut
	$(SUDO) TEST_WORKDIR_PREFIX=$(TEST_WORKDIR_PREFIX) $(CARGO) test --test '*' $(CARGO_COMMON) -- --nocapture --test-threads=8

docker-nydus-smoke:
	docker build -t nydus-smoke --build-arg RUST_TARGET=${RUST_TARGET_STATIC} misc/nydus-smoke
	docker run --rm --privileged ${CARGO_BUILD_GEARS} \
		-e TEST_WORKDIR_PREFIX=$(TEST_WORKDIR_PREFIX) \
		-v ~/.cargo:/root/.cargo \
		-v $(TEST_WORKDIR_PREFIX) \
		-v ${current_dir}:/nydus-rs \
		nydus-smoke

# TODO: Nydusify smoke has to be time consuming for a while since it relies on musl nydusd and nydus-image.
# So musl compilation must be involved.
# And docker-in-docker deployment involves image building?
docker-nydusify-smoke: docker-static
	$(call build_golang,$(NYDUSIFY_PATH),make build-smoke)
	docker build -t nydusify-smoke misc/nydusify-smoke
	docker run --rm --privileged \
		-e BACKEND_TYPE=$(BACKEND_TYPE) \
		-e BACKEND_CONFIG=$(BACKEND_CONFIG) \
		-v $(current_dir):/nydus-rs $(dind_cache_mount) nydusify-smoke TestSmoke

docker-nydusify-image-test: docker-static
	$(call build_golang,$(NYDUSIFY_PATH),make build-smoke)
	docker build -t nydusify-smoke misc/nydusify-smoke
	docker run --rm --privileged \
		-e BACKEND_TYPE=$(BACKEND_TYPE) \
		-e BACKEND_CONFIG=$(BACKEND_CONFIG) \
		-v $(current_dir):/nydus-rs $(dind_cache_mount) nydusify-smoke TestDockerHubImage

# Run integration smoke test in docker-in-docker container. It requires some special settings,
# refer to `misc/example/README.md` for details.
docker-smoke: docker-nydus-smoke docker-nydusify-smoke

contrib-build: nydusify ctr-remote nydus-overlayfs docker-nydus-graphdriver

contrib-release: nydusify-release ctr-remote-release \
			    nydus-overlayfs-release docker-nydus-graphdriver-release

contrib-test: nydusify-test ctr-remote-test \
				nydus-overlayfs-test docker-nydus-graphdriver-test

contrib-clean: nydusify-clean ctr-remote-clean \
				nydus-overlayfs-clean docker-nydus-graphdriver-clean

contrib-install:
	@sudo mkdir -m 755 -p $(INSTALL_DIR_PREFIX)
	@sudo install -m 755 contrib/ctr-remote/bin/ctr-remote $(INSTALL_DIR_PREFIX)/ctr-remote
	@sudo install -m 755 contrib/docker-nydus-graphdriver/bin/nydus-graphdriver $(INSTALL_DIR_PREFIX)/nydus-overlayfs
	@sudo install -m 755 contrib/nydus-overlayfs/bin/nydus-overlayfs $(INSTALL_DIR_PREFIX)/nydus-overlayfs
	@sudo install -m 755 contrib/nydusify/cmd/nydusify $(INSTALL_DIR_PREFIX)/nydusify

nydusify:
	$(call build_golang,${NYDUSIFY_PATH},make)

nydusify-release:
	$(call build_golang,${NYDUSIFY_PATH},make release)

nydusify-test:
	$(call build_golang,${NYDUSIFY_PATH},make test)

nydusify-clean:
	$(call build_golang,${NYDUSIFY_PATH},make clean)

ctr-remote:
	$(call build_golang,${CTR-REMOTE_PATH},make)

ctr-remote-release:
	$(call build_golang,${CTR-REMOTE_PATH},make release)

ctr-remote-test:
	$(call build_golang,${CTR-REMOTE_PATH},make test)

ctr-remote-clean:
	$(call build_golang,${CTR-REMOTE_PATH},make clean)

nydus-overlayfs:
	$(call build_golang,${NYDUS-OVERLAYFS_PATH},make)

nydus-overlayfs-release:
	$(call build_golang,${NYDUS-OVERLAYFS_PATH},make release)

nydus-overlayfs-test:
	$(call build_golang,${NYDUS-OVERLAYFS_PATH},make test)

nydus-overlayfs-clean:
	$(call build_golang,${NYDUS-OVERLAYFS_PATH},make clean)

docker-nydus-graphdriver:
	$(call build_golang,${DOCKER-GRAPHDRIVER_PATH},make)

docker-nydus-graphdriver-release:
	$(call build_golang,${DOCKER-GRAPHDRIVER_PATH},make release)

docker-nydus-graphdriver-test:
	$(call build_golang,${DOCKER-GRAPHDRIVER_PATH},make test)

docker-nydus-graphdriver-clean:
	$(call build_golang,${DOCKER-GRAPHDRIVER_PATH},make clean)

docker-static:
	docker build -t nydus-rs-static --build-arg RUST_TARGET=${RUST_TARGET_STATIC} misc/musl-static
	docker run --rm ${CARGO_BUILD_GEARS} -e RUST_TARGET=${RUST_TARGET_STATIC} --workdir /nydus-rs -v ${current_dir}:/nydus-rs nydus-rs-static

docker-example: all-static-release
	cp ${current_dir}/target/${RUST_TARGET_STATIC}/release/nydusd misc/example
	cp ${current_dir}/target/${RUST_TARGET_STATIC}/release/nydus-image misc/example
	cp contrib/nydusify/cmd/nydusify misc/example
	docker build -t nydus-rs-example misc/example
	@cid=$(shell docker run --rm -t -d --privileged $(dind_cache_mount) nydus-rs-example)
	@docker exec $$cid /run.sh
	@EXIT_CODE=$$?
	@docker rm -f $$cid
	@exit $$EXIT_CODE
