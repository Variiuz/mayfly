# Changelog

## [0.10.0](https://github.com/Variiuz/mayfly/compare/v0.9.0...v0.10.0) (2026-11-11)


### Features

* the binary runs every slot; mayfly.sh and the engine switch are gone ([#42](https://github.com/Variiuz/mayfly/issues/42)) ([3da6cce](https://github.com/Variiuz/mayfly/commit/3da6cce7f064758fe70df13a7bcc22e840d91260))

## [0.9.0](https://github.com/Variiuz/mayfly/compare/v0.8.1...v0.9.0) (2026-11-10)


### Features

* mayfly stats, per-slot jobs, idle and stuck guests and errors from the journal ([#38](https://github.com/Variiuz/mayfly/issues/38)) ([338b1c7](https://github.com/Variiuz/mayfly/commit/338b1c79c0effcb92d403ac5fc829cb5b814f9e7))

## [0.8.1](https://github.com/Variiuz/mayfly/compare/v0.8.0...v0.8.1) (2026-11-10)


### Bug Fixes

* drain trap only flags the stop, the loop does the rest ([#35](https://github.com/Variiuz/mayfly/issues/35)) ([eac6ead](https://github.com/Variiuz/mayfly/commit/eac6eadf6220bf588d475e8e72a90f3f87c2e449))

## [0.8.0](https://github.com/Variiuz/mayfly/compare/v0.7.0...v0.8.0) (2026-11-10)


### ⚠ BREAKING CHANGES

* a host whose current template was not built by Mayfly stops starting jobs. Build and promote a Mayfly template before updating.

### Bug Fixes

* clear errors for a missing lxc image and a failed old-template removal ([#32](https://github.com/Variiuz/mayfly/issues/32)) ([157aa48](https://github.com/Variiuz/mayfly/commit/157aa488fc48cc532d43a99c5048bf7a9d0a3e7a))
* drop the polara-ci template fallback ([#14](https://github.com/Variiuz/mayfly/issues/14)) ([e7cebf9](https://github.com/Variiuz/mayfly/commit/e7cebf9b46c341a8bafb859e0ea24e93cd2cf144))

## [0.7.0](https://github.com/Variiuz/mayfly/compare/v0.6.0...v0.7.0) (2026-11-10)


### Features

* rust slot loops for lxc and pod slots behind engine=rust ([#30](https://github.com/Variiuz/mayfly/issues/30)) ([80e9603](https://github.com/Variiuz/mayfly/commit/80e96037c3a1e34fbfc53fda9e08b6f0addaaf51))

## [0.6.0](https://github.com/Variiuz/mayfly/compare/v0.5.0...v0.6.0) (2026-11-10)


### Features

* rust slot loop for vm slots behind engine=rust ([#29](https://github.com/Variiuz/mayfly/issues/29)) ([c42ef03](https://github.com/Variiuz/mayfly/commit/c42ef03f71fb73efb236444c6d9d6a95ee1e8fdd))
* rust template build, canary, promote and list ([#27](https://github.com/Variiuz/mayfly/issues/27)) ([e8c1e44](https://github.com/Variiuz/mayfly/commit/e8c1e44cbe8ef9978d5de06b403365fb76f064b1))

## [0.5.0](https://github.com/Variiuz/mayfly/compare/v0.4.1...v0.5.0) (2026-11-10)


### Features

* mayfly install in rust; install.sh only fetches and verifies ([#26](https://github.com/Variiuz/mayfly/issues/26)) ([48ae0de](https://github.com/Variiuz/mayfly/commit/48ae0de9a759ae2d5a53fb0ca6b5548699452921))
* rust gc, enable and disable ([#23](https://github.com/Variiuz/mayfly/issues/23)) ([326478e](https://github.com/Variiuz/mayfly/commit/326478ef1d0181ae782ba91a183927c77359940e))


### Bug Fixes

* **pod:** back off when a pod never gets ready ([#24](https://github.com/Variiuz/mayfly/issues/24)) ([b82ae57](https://github.com/Variiuz/mayfly/commit/b82ae57ea907ec11aca9953756a818cfb5644df7))

## [0.4.1](https://github.com/Variiuz/mayfly/compare/v0.4.0...v0.4.1) (2026-11-10)


### Bug Fixes

* **template:** run the profile from a directory runner can read ([#21](https://github.com/Variiuz/mayfly/issues/21)) ([2617d3f](https://github.com/Variiuz/mayfly/commit/2617d3f8516ee3323bde5e511e48f464e2a5ad61))
* **update:** the timer never installs a breaking release ([#20](https://github.com/Variiuz/mayfly/issues/20)) ([24e30a8](https://github.com/Variiuz/mayfly/commit/24e30a8341fae602310180e0e0bfe806465549f7))

## [0.4.0](https://github.com/Variiuz/mayfly/compare/v0.3.0...v0.4.0) (2026-11-10)


### Features

* mayfly update and a daily update timer ([#18](https://github.com/Variiuz/mayfly/issues/18)) ([c148eb7](https://github.com/Variiuz/mayfly/commit/c148eb7f214f366684a3fa0324d3c8e6fe68c000))
* **template:** run builds in their own systemd unit ([#17](https://github.com/Variiuz/mayfly/issues/17)) ([c302027](https://github.com/Variiuz/mayfly/commit/c3020274040debd2d28f4a0351e0de08ab9c0125))

## [0.3.0](https://github.com/Variiuz/mayfly/compare/v0.2.0...v0.3.0) (2026-11-10)


### Features

* rust binary for version, help, status and check ([#10](https://github.com/Variiuz/mayfly/issues/10)) ([a478fe3](https://github.com/Variiuz/mayfly/commit/a478fe34d3949d5585fc6ceb9487d1f1efef37bb))
* **slot:** replace a guest whose runner never comes online ([#16](https://github.com/Variiuz/mayfly/issues/16)) ([7765b3a](https://github.com/Variiuz/mayfly/commit/7765b3aaa97978c54933675c226f4370acbd0dfd))

## [0.2.0](https://github.com/Variiuz/mayfly/compare/v0.1.3...v0.2.0) (2026-11-10)


### Features

* **check:** warn about pool.env lines that are not plain KEY=value ([#5](https://github.com/Variiuz/mayfly/issues/5)) ([4740e40](https://github.com/Variiuz/mayfly/commit/4740e4073b2078df6475f2bdaadded8b812b7712))

## [0.1.3](https://github.com/Variiuz/mayfly/compare/v0.1.2...v0.1.3) (2026-11-10)


### Bug Fixes

* **template:** export template.env so the profile's provision.sh sees it ([#11](https://github.com/Variiuz/mayfly/issues/11)) ([16ba093](https://github.com/Variiuz/mayfly/commit/16ba093b5f0c5b6e07043d710e61f331a345dc2b))

## [0.1.2](https://github.com/Variiuz/mayfly/compare/v0.1.1...v0.1.2) (2026-11-10)


### Bug Fixes

* **template:** lock builds and only report a template that was really built ([#8](https://github.com/Variiuz/mayfly/issues/8)) ([d71c1ce](https://github.com/Variiuz/mayfly/commit/d71c1cea0e3b0a9a3e4024879ee8bfb6beb607c9))

## [0.1.1](https://github.com/Variiuz/mayfly/compare/v0.1.0...v0.1.1) (2026-11-10)


### Bug Fixes

* run jobs on polara-ci templates and show GitHub runner state in status ([#6](https://github.com/Variiuz/mayfly/issues/6)) ([b897484](https://github.com/Variiuz/mayfly/commit/b8974843e6018f6d80cb5503e8d1bfe1af2f8313))

## 0.1.0 (2026-11-10)


### Features

* Mayfly, throwaway GitHub Actions runners on Proxmox ([9047ab4](https://github.com/Variiuz/mayfly/commit/9047ab46d1a1cf389498adaedf5a61ea58691c1b))


### Bug Fixes

* **install:** ignore old builds' help text in the "was" version ([7fea2c0](https://github.com/Variiuz/mayfly/commit/7fea2c084c7b0a21541490e9a27dd082002f4379))
