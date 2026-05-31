# pixhub-rs

pixiv 与 douyin 资源后端 API — Rust 重写版

基于 axum + SQLite + notify 构建，提供随机资源获取、文件查询、在线浏览等功能。

## 快速开始

```bash
cp .env.example .env
# 编辑 .env 配置资源路径等
cargo run --release
```

## 接口

| 端点 | 说明 |
|------|------|
| `GET /` | API 主页 |
| `GET /pixiv/artworks/random` | 随机 pixiv 图片信息 |
| `GET /pixiv/artworks/info?list={文件名}` | 查询图片信息 |
| `GET /pixiv/artworks/view` | 直接查看随机图片 |
| `GET /pixiv/artworks/file/{文件名}` | 获取图片文件 |
| `GET /plus/artworks/random` | 随机视频信息 |
| `GET /plus/artworks/info?list={作者\|all}` | 查询视频信息 |
| `GET /plus/artworks/view` | 直接查看随机视频 |
| `GET /plus/artworks/file/{文件名}` | 获取视频文件 |

## 许可

Apache-2.0
