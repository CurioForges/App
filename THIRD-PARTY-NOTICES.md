# 第三方声明

Atelier 使用了下列开源组件，各自遵循其原始许可。分发本软件时请保留本文件。

| 组件 | 许可 | 用途 |
| :-- | :-- | :-- |
| Tauri | MIT / Apache-2.0 | 桌面外壳与进程通信 |
| React | MIT | 界面渲染 |
| Vite | MIT | 前端构建 |
| serde / serde_json | MIT / Apache-2.0 | 配置与消息序列化 |
| reqwest | MIT / Apache-2.0 | HTTP 客户端 |
| sysinfo | MIT | 进程与系统信息 |
| windows-sys | MIT / Apache-2.0 | Windows API 绑定 |

模型权重与各能力的第三方依赖由各自的发布包携带，声明见对应仓库的 NOTICES。

## 关于模型权重

Atelier 自身**不包含**模型权重。权重在首次使用时从 Hugging Face 等公开渠道下载，
受限（gated）模型需要使用者用自己的令牌并先在网页上同意对应许可。