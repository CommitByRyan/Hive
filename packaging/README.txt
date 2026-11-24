Hive 0.1.0 - Windows x86_64

安装
  1. 解压发布包到目标目录。
  2. 进入解压出的 hive-0.1.0-windows-x86_64 目录，运行:
       .\bin\hive.exe doctor
  3. 将 hive.env.example 复制为 .env，按需填写 API Key 与模型。
     不要提交或分享真实 .env。

配置
  - 进程环境变量优先于 .env，.env 优先于内置默认值。
  - 未知 HIVE_* 键会被拒绝并报告行号；非 HIVE_* 键忽略。
  - API Key 只显示 set/unset，不会打印真实值。
  - 默认监听 127.0.0.1:8765，默认数据目录 data，沙箱目录 sandbox。

启动
  - 在安装目录运行:
       .\start-hive.ps1
    或直接运行:
       .\bin\hive-server.exe
  - 启动脚本只切换目录并启动服务，不修改 PATH 或注册表。

升级
  - 停服后解压新发布包，覆盖程序文件；保留 data、sandbox、.env 与用户文件。
  - 服务启动时若检测到旧 schema，会先写入 data\backups\hive.db.bak-v<旧版本>-<时间戳>。
  - 备份失败时服务拒绝启动。

回滚
  - 停服。
  - 用 data\backups\hive.db.bak-* 覆盖 data\hive.db。
  - 退回到之前的发布包并启动。
  - 迁移只向前，不提供 down migration；没有备份时不能安全回滚。

校验
  - 发布包内 SHA256SUMS.txt 逐文件列出哈希。
  - 清单缺失或包含未列出文件时，安装 helper 会拒绝安装。
  - PowerShell: Get-FileHash -Algorithm SHA256 .\bin\hive.exe
