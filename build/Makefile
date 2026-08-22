# OTWONO image build.
#
#   make probe                          environment probe — run this first
#   make TARGET=amd64-qemu rootfs       stages 10..30
#   make TARGET=amd64-qemu image        stages 10..50
#   make TARGET=amd64-qemu boot-test    stage 60 (QEMU boot, TCG-safe timeouts)
#   make TARGET=arm64-qemu image
#   make list-targets
#   make clean / make distclean
#
# Every stage is idempotent and writes to $(OUT)/<target>/. Nothing is committed from
# there — see .gitignore.

SHELL       := /bin/bash
.SHELLFLAGS := -eu -o pipefail -c
.DEFAULT_GOAL := help

BUILD_DIR := $(patsubst %/,%,$(dir $(abspath $(lastword $(MAKEFILE_LIST)))))
REPO_ROOT := $(patsubst %/,%,$(dir $(BUILD_DIR)))
OUT       ?= $(REPO_ROOT)/out
TARGET    ?= amd64-qemu
RECIPE    := $(BUILD_DIR)/recipes/$(TARGET).toml
STAGES    := $(BUILD_DIR)/stages
TARGET_OUT := $(OUT)/$(TARGET)

# Reproducibility: a fixed epoch unless the caller pins one.
SOURCE_DATE_EPOCH ?= 1767225600
export SOURCE_DATE_EPOCH OUT TARGET RECIPE REPO_ROOT BUILD_DIR TARGET_OUT

.PHONY: help probe list-targets check-recipe rootfs image boot-test verify clean distclean \
        stage-10 stage-20 stage-30 stage-40 stage-50 stage-60 host-tools

help:
	@sed -n '1,/^$$/p' $(firstword $(MAKEFILE_LIST)) | sed 's/^# \{0,1\}//'
	@echo
	@echo "Current: TARGET=$(TARGET)  OUT=$(OUT)"

list-targets:
	@ls -1 $(BUILD_DIR)/recipes/*.toml | xargs -n1 basename | sed 's/\.toml$$//'

probe:
	@$(REPO_ROOT)/tools/probe-env.sh

check-recipe:
	@test -f "$(RECIPE)" || { \
	  echo "no recipe for TARGET=$(TARGET); available:" >&2; \
	  $(MAKE) --no-print-directory list-targets >&2; exit 1; }

# Host-side artifacts that go *into* the image: the OTWONO binaries for the target arch.
host-tools: check-recipe
	@$(STAGES)/05-host-tools.sh

stage-10: check-recipe ; @$(STAGES)/10-bootstrap.sh
stage-20: stage-10     ; @$(STAGES)/20-base-config.sh
stage-30: stage-20 host-tools ; @$(STAGES)/30-otwono.sh
stage-40: stage-30     ; @$(STAGES)/40-kernel.sh
stage-50: stage-40     ; @$(STAGES)/50-image.sh
stage-60: stage-50     ; @$(STAGES)/60-verify.sh

rootfs:    stage-30
image:     stage-50
boot-test: stage-60
verify:    stage-60

clean:
	rm -rf "$(TARGET_OUT)"

distclean:
	rm -rf "$(OUT)"
	cd "$(REPO_ROOT)" && cargo clean
