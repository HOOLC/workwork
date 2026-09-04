# Rust 构建缓存约束

## 目标

workspace 内直接依赖同一个第三方 crate 时，使用同一版本和同一组 feature，避免单包和多包构建仅因 member 的声明不同而生成额外编译变体。

## 约束

- feature 曾经存在分叉的公共依赖在根 `Cargo.toml` 的 `[workspace.dependencies]` 中声明一次，取 workspace 当前实际需要的 feature 并集。
- member 通过 `dependency.workspace = true` 继承，不再声明自己的版本、默认 feature 或 feature 子集。
- 不添加仅用于影响 feature 解析的依赖或 crate。
- 不把 debug/release、目标平台、编译器版本、源代码和 `rustflags` 不同的产物视为同一缓存项。

当前统一的直接依赖是 `reqwest`、`tokio`、`futures-util`、`serde` 和 `tokio-tungstenite`。

## 边界

稳定 Cargo 根据一次命令实际选中的包统一 feature。`[workspace.dependencies]` 统一直接依赖声明，但不会强制没有直接使用某个依赖的 member 激活该依赖，也不会改写传递依赖的 feature。

## 验证

`test/workspace-cargo-features.test.ts` 检查上述依赖只有一份 workspace 声明，并且 member 的直接使用全部继承该声明。
