# 计算服务 §10 运维边界落地方案

> 日期：2026-11-12。同日按 owner 决定修订选型。只写方案和脚本草案，不改 Rust，不部署，不开云账号。
> 权威顺序：定稿 `docs/design/compute-service-design.md`（定稿 1.0）高于本文。本文把 §10 写成可执行的运维方案。代码现状以本工作区当天读到的源码为准，行号也是这一天的。上一轮勘察见 `docs/design/compute-service-code-recon.md`。
> 跨境和法律定性以 `docs/design/compute-service-compliance-research.md` 为准。本文不另下法律结论。那份研究写明它不是法律意见，本文同样不是。
> 外部事实后面附 URL 和查询日期。查不到或来源不够的，写「未核实」。

## 1. 选型结论

P0 维持设计稿：一个活动写节点，SQLite 放在持久卷上，WAL 持续复制到对象存储。这是可恢复，不是高可用。目标仍是 RPO ≤ 1 分钟、RTO ≤ 30 分钟。

**复制工具用 Litestream 0.5.x，钉在已经发布的 v0.5.18。** 它按现有 SQLite 文件做边车复制，应用继续用 rusqlite，不必改写入 API。默认大约每秒把新变更送到副本，灾难宕机时丢掉的是还没复制出去的那一小段；正常停机时它会先把未同步的 WAL 推完再退出。这和「单写、可恢复、RPO 以分钟计」相符。见第 2 节。

**不用 LiteFS 做 P0。** 它是多节点复制文件系统，目标是高可用和就近读。设计稿明确不做多活。Fly 的文档把它放在支持范围之外，并警告自动停启机器可能让旧数据拿到租约、丢掉更新。

**不用 rqlite 做 P0。** 官方文档把它写成要连接的集群，用来做高可用，不是挂在现有库文件旁边的复制进程。接进去就要换掉现在的 rusqlite 打开方式。这是选型判断，不是 rqlite 文档里的一句禁令。

**不用 sqlite3_rsync 做主备份。** 它把副本做成命令开始那一瞬间的一致快照，适合定时拷贝，不是持续把 WAL 推进对象存储。可以留作演练时的第二份拷贝，不承担 1 分钟 RPO。

**计算节点放在新加坡，库文件放在该节点的持久卷上。** 卷的每日快照只是第二道，达不到 1 分钟 RPO，也不能代替对象存储副本。Fly 自己也写明卷快照不应当是唯一备份。

**对象存储副本优先选能说清「数据在新加坡」、S3 兼容、并且 Litestream 0.5.x 有文档支持的服务。** 各家的区域、卷、价格和驻留原句在第 2.4 节。价格是公开标价，不是报价。查询日期都是 2026-11-12。

- AWS：按 owner 2026-11-12 决定排除。
- **首选：DigitalOcean SGP1。** 库文件放在 SGP1 的标准卷上，WAL 副本用同区 Spaces，不打开 CDN。Litestream 0.5 把 `*.digitaloceanspaces.com` 列为自动识别的 S3 兼容端点，区域表写明新加坡是 `sgp1.digitaloceanspaces.com`。同区 Droplet 访问 SGP1 Spaces 的出站，计费说明里不计 Spaces 出站费。桶在创建时选定数据中心，之后不能改区域。官方没有「对象字节不会离开该数据中心」这种句子。CDN 接入点是全球的，备份桶不能开。
- **备选：Fly.io 区域 `sin` 上的单写 Machine 加卷，副本用 Tigris 单区域 `sin`。** Litestream 0.5.3 起识别 Tigris。Tigris 写明单区域桶的对象数据不会离开所选区域。对象键、大小和系统元数据会复制到所有区域。默认 Global 桶按访问搬数据，不能用。Litestream 指南里的 `fly storage create` 没有写区域，不能拿那个默认桶当生产副本。
- **不要把 Cloudflare R2 当作唯一副本。** R2 的司法辖区只有欧盟、美国和 FedRAMP，没有新加坡。`apac` 只是创建桶时的位置提示，不是驻留保证。儿童相关账本放到一个说不清落点的桶里，就是合规研究里「新加坡再向外送一跳」还没做决定的情况。
- **不要用 Hetzner Object Storage 做这份副本。** 官方地点表里，对象存储只在法尔肯施泰因、纽伦堡和赫尔辛基，不在新加坡。从新加坡机器写过去，账本离开新加坡。

阿里云国际站新加坡有 Litestream 原生 `type: oss`（v0.5.3 起），创建桶的文档把 Region 写成数据的物理位置。它不进首选或备选：示例区域是 `cn-hangzhou` 和 `us-west-1`，没有点名新加坡，本次未实测；走 S3 兼容层时，官方写 boto3 的 Signature V4 与分块编码不兼容。新加坡云盘和 OSS 单价，本次抓到的价目表不能确认绑在新加坡上。Vultr、Akamai/Linode、腾讯云国际站、Google Cloud 的缺口也在第 2.4 节，同样不进首选或备选。

Litestream 不绑定某一个云的「新加坡产品」。区域是副本 URL 里的 endpoint 和 region。只要该 endpoint 在新加坡，复制间隔就跟区域无关。

**本机演练**用 Litestream 的 `file://` 副本，不连外网、不用真实密钥。脚本是 `scripts/compute-ops/rehearse-litestream-restore.sh`。2026-11-12 已在本机跑通（第 5 节）。MinIO 镜像当天拉不下来，所以没有提交 MinIO 脚本；MinIO 步骤写在第 4 节，标成未执行。

## 2. 核实过的外部事实

查询日期都是 2026-11-12，除非某条另注。

### 2.1 Litestream

- 最新发布是 v0.5.18，发布者 benbjohnson，日期 2026-11-03。支持 Linux（amd64、arm64、armv6、armv7）和 macOS（amd64、arm64）。Windows 二进制有，但发行说明写明不是官方支持平台。https://github.com/benbjohnson/litestream/releases
- 文档站把 v0.5.x 标成当前维护线，有功能和修复；v0.3.14 是上一线。https://litestream.io/tips/
- 维护活动：GitHub 提交历史在 2026-10-20 仍有提交（例如保持读锁）。https://github.com/benbjohnson/litestream/commits/main/ 本次只看到检索摘要，没有逐条打开每个 commit。
- 复制是异步的。默认大约每秒把新变更复制到副本（该页以 S3 兼容副本为例）。这段还没复制出去的数据，在整机灾难时会丢。收到关闭信号时，会尽量先把未同步的 WAL 同步到副本再退出。同步复制在路线图上，还没有。https://litestream.io/tips/ （Data loss window）
- 配置参考里，时间类全局默认可以写 `sync-interval: 1s`。每个库还有 `monitor-interval`，默认 `1s`。https://litestream.io/reference/config/
- 恢复粒度是整个 LTX 文件，不会套用半个文件。保留策略删掉细粒度 L0 之后，能恢复到的时间点变粗。L0 不能永久保留（`l0-retention: 0` 会被拒绝）。https://litestream.io/how-it-works/
- 快照默认每 24 小时一次，保留 24 小时。写得勤时，文档建议把 `snapshot.interval` 降到 `1h`，否则恢复要重放很多增量，恢复变慢。https://litestream.io/tips/
- 关闭时的最后一次同步默认最多等 30 秒，每 500 毫秒重试。https://litestream.io/reference/config/ （Shutdown sync）
- 只支持 WAL。应用没开的话，Litestream 自己会把库改成 WAL。https://litestream.io/tips/
- 它自己也是一个会短时间拿写锁的进程（做 checkpoint）。应用连接要设 `PRAGMA busy_timeout = 5000`。https://litestream.io/tips/
- WAL 模式下可以把 `synchronous` 设成 `NORMAL`。文档说这样只在 WAL 要 checkpoint 回主文件时才 fsync，并认为 WAL 只追加所以这样是安全的。同一页写明：还没复制出去的窗口会丢数据。https://litestream.io/tips/
- 高写入（文档举例：每秒数万个小事务）时，应用自己的 checkpoint 可能插在两次 Litestream checkpoint 之间，导致漏掉一段 WAL。Litestream 发现后会做一次全量快照。文档因此建议 `PRAGMA wal_autocheckpoint = 0`，让 Litestream 管 checkpoint。https://litestream.io/tips/
- 原地 `VACUUM` 会重写整个库文件，页跟踪失效。要用 `VACUUM INTO`。https://litestream.io/docs/troubleshooting
- 两个应用同时往同一个桶和路径复制，可能使恢复失败。只能有一个复制者。https://litestream.io/tips/
- 恢复命令是 `litestream restore`。可以按时间点：`litestream restore -timestamp ... -o restored.db`。恢复后应用自己跑 `PRAGMA integrity_check`。https://litestream.io/docs/troubleshooting
- v0.5.7 起有 `validation` 块，按间隔检查 LTX 的顺序和是否连续（空隙、重叠、乱序）。默认关闭。这不是把恢复出来的库跑一遍 `integrity_check`。https://litestream.io/reference/config/ （Validation）
- v0.5.18 的变更列表里有一条「opt-in PutObject checksums」。本次没有在配置参考里看到对应的 YAML 键名，键名未核实。https://github.com/benbjohnson/litestream/releases
- S3 兼容：文档列出 MinIO、DigitalOcean Spaces、Hetzner（`*.your-objectstorage.com`，v0.5.9 起自动探测）、Cloudflare R2、Tigris 等。R2 的 region 写 `auto`，欧盟司法辖区用 `<ACCOUNT_ID>.eu.r2.cloudflarestorage.com`（v0.5.8）。没有单独的「新加坡区域」开关；新加坡取决于桶所在的 endpoint。https://litestream.io/guides/s3-compatible/ 与 https://litestream.io/reference/config/
- 同一配置参考还有不是 S3 副本类型的后端：`type: gs` 是 Google Cloud Storage，用应用默认凭证；`type: oss` 是阿里云 OSS，用官方 SDK（v0.5.3）。这两条不走 S3 兼容端点。https://litestream.io/reference/config/
- 多个 SQLite 连接可以同时使用这个库。Litestream 的要求是 WAL，以及应用在它 checkpoint 时不要立刻失败。它没有要求进程里只有一条连接。高写入下的限制是上面的 autocheckpoint，不是「禁止多连接」。https://litestream.io/tips/

### 2.2 替代方案

- LiteFS：FUSE，按事务把 SQLite 复制到多台机器，目标是高可用和低延迟读。Litestream 官方替代方案页写：LiteFS 面向高可用，Litestream 面向灾难恢复；两者同一批作者。https://litestream.io/alternatives/
- Fly 文档：LiteFS 不在支持范围内；不要和 Machines 的 autostop/autostart 一起用，否则可能回滚并永久丢数据。项目仍是 pre-1.0，并建议另做异地备份。https://fly.io/docs/litefs/
- GitHub 发行页在本次检索里把 v0.5.14 标成 Latest，可见日期片段是「22 Apr」，年份没有在摘录里展开。年份未核实。https://github.com/superfly/litefs/releases
- sqlite3_rsync：官方工具。副本是命令开始时原点的一致快照；命令进行中原点上的新写入不会进这次副本。3.50.0（2025-05-29）起不再要求两端都是 WAL、也不再要求页大小相同。一次只同步一个库。https://sqlite.org/rsync.html
- 论坛记录过 v3.50.4 在写入负载下损坏副本、后来有修复提交。本次没有核对 3.51 是否包含该修复。未核实「当前版本已经没有这个问题」。https://sqlite.org/forum/info/46753431d4
- rqlite：文档有独立的集群章节，标题是创建和管理集群以获得高可用。https://rqlite.io/docs/clustering/ （页脚 Last modified July 25, 2026）

### 2.3 SQLite 本身

- WAL 默认在 WAL 文件大约到 1000 页时自动 checkpoint。可以用 `wal_autocheckpoint` 关掉。https://sqlite.org/wal.html
- `synchronous=FULL` 是回滚日志的默认，在 WAL 下是 ACID 的。`synchronous=NORMAL` 在 WAL 下不会因为断电损坏库，但会失去持久性：已提交的事务可能在断电或系统崩溃后回滚。应用崩溃不会因此丢掉已提交事务。https://sqlite.org/pragma.html#pragma_synchronous
- `busy_timeout` 是每个连接自己的忙等，单位毫秒。https://sqlite.org/pragma.html#pragma_busy_timeout

### 2.4 新加坡的卷和对象存储

价格是公开标价，不含税，不是报价。没有下单。

**Fly.io**

- 有新加坡区域 `sin`。计算价格是阿什本价格乘区域系数；`sin` 的系数是 1.269。`shared-cpu-1x · 256MB` 在 sin 的月价示例是 $2.78。卷和快照不加这个系数。https://fly.io/pricing.md （检索摘录，2026-11-12）
- 卷 $0.15/GB/月，按划定容量计费，卸下或机器停了也计费。快照 $0.08/GB/月，每月前 10GB 免费。新卷默认每天自动快照、保留 5 天，可改成 1 到 60 天或关掉。快照按实际存的数据计费，增量。https://fly.io/docs/about/pricing/ 与 https://fly.io/docs/volumes/snapshots/
- 卷属于一个应用，落在一台机器、一个区域上，不是网络共享盘。每天的快照可能没有最新数据，不能当唯一备份。可以从快照拉出一个不小于原卷的新卷。https://fly.io/docs/volumes/overview/ 同页原句：snapshots shouldn’t be your primary backup method；Daily automatic snapshots may not have your latest data。

**Tigris（Fly 上的对象存储）**

- S3 兼容。Litestream 配置参考把 Tigris (Fly.io) 列在签名表里：`sign-payload: true`，`require-content-md5: false`，并写明识别端点 `fly.storage.tigris.dev`、region 用 `auto`（v0.5.3）。指南在 v0.5.4。https://litestream.io/reference/config/ 与 https://litestream.io/guides/tigris/
- 区域表有 Asia-Pacific `sin` Singapore。https://www.tigrisdata.com/docs/concepts/regions/ （页面可见 2026-09-25 一带的发布标记；查询 2026-11-12）
- 桶位置在创建时选定。单区域：object data never leaves the chosen region。Global 按访问分布，not suitable for strict data residency requirements。Residency applies to object data. Object keys, sizes and other system metadata, along with bucket settings, are replicated to all regions. https://www.tigrisdata.com/docs/buckets/locations/ （页上可见 2026-11-11 的更新标记）
- 自定义元数据还会复制到同一地理的灾难恢复区域。例子是法兰克福对应伦敦和阿姆斯特丹。新加坡 `sin` 的灾难恢复区域叫什么：未核实。
- Litestream 指南的创建命令是 `fly storage create`，没有区域参数。那不等于单区域 `sin`。博客示例是 `tigris mk my-bucket --locations sin`。https://www.tigrisdata.com/blog/multi-region-dual-region-buckets/ （文内日期 2026-03-24）Litestream 上传时会不会另送区域头、从而改掉桶级位置：未核实。
- 价格：Global / Single-Region 标准存储 $0.02/GB/月，Class A $0.005/1000 次，Class B $0.0005/1000 次，出站免费。每月含 5 GB、1 万次 Class A、10 万次 Class B。https://www.tigrisdata.com/pricing.md （页上日期 2026-10-22）币种是美元。
- 本次没有用真实 Tigris 桶做复制。

**Cloudflare R2**

- 标准存储 $0.015 / GB-月，Class A $4.50 / 百万次，Class B $0.36 / 百万次，出站免费。每月含 10 GB、100 万次 Class A、1000 万次 Class B。https://developers.cloudflare.com/r2/pricing/ （页上 Last updated Oct 30, 2026）
- 位置提示含 `apac`，那是期望的主要访问地，不是驻留保证。司法辖区只有 `eu`、`fedramp`、`us`。辖区保证对象存在该辖区内。桶一旦创建，辖区不能改。https://developers.cloudflare.com/r2/reference/data-location/ 与 https://developers.cloudflare.com/changelog/post/2026-08-17-r2-us-jurisdiction/
- 没有新加坡辖区。未核实 R2 在 `apac` 提示下会把字节放进哪一个具体数据中心。

**DigitalOcean**

- Spaces 在 SGP1。https://docs.digitalocean.com/products/spaces/details/availability/
- Spaces 标准存储订阅 $5/月，含全部桶合计 250 GiB；超出 $0.02 / GiB-月。含 1,024 GiB 出站，超出 $0.01 / GiB。SGP1 的 Spaces 到 SGP1 的 Droplet 出站不计费。https://docs.digitalocean.com/products/spaces/details/pricing/ （last_updated 2026-10-26）
- 卷 $0.10 / GiB-月，1 GiB 到 16 TiB，不挂上也计费。https://docs.digitalocean.com/products/volumes/details/pricing/ （Last verified 11 Nov 2026）
- 卷快照 $0.06 / GiB-月。https://docs.digitalocean.com/products/snapshots/details/pricing/ （Last verified 12 Nov 2026）
- 卷快照是崩溃一致：冻结某一刻已写到盘上的块，不是应用事务一致。https://docs.digitalocean.com/products/snapshots/how-to/snapshot-volumes/
- 标准卷在 SGP1 可用。卷是区域资源，只能在同一数据中心的 Droplet 之间移动。高性能卷的私有预览名单里没有 SGP1。https://docs.digitalocean.com/products/volumes/details/availability/ （Generated on 12 Nov 2026）
- Spaces 在 SGP1 的标准存储和冷存储都标了 Available。https://docs.digitalocean.com/products/spaces/details/availability/ （Generated on 12 Nov 2026）
- 创建桶：Choose a datacenter region，所选数据中心会进 endpoint。Once a bucket is created, its region cannot be changed. https://docs.digitalocean.com/products/spaces/how-to/create/ 与 https://docs.digitalocean.com/products/spaces/how-to/transfer-between-regions/ 没有查到「对象字节不会离开该数据中心」的原句。
- CDN 在每个有 Spaces 的区域都能开。接入点表的 Asia 一行包含 Singapore，也包含曼谷、东京、香港和其他城市。https://docs.digitalocean.com/products/spaces/details/availability/ 备份桶不要开 CDN。
- 数据中心表有 SGP1，Singapore。https://docs.digitalocean.com/platform/regional-availability/ Droplet 各档在 SGP1 的勾选，本次打开的 markdown 表格没有把图标转成文字，不逐档写成已可用。SGP1 的 Droplet 月价：未核实。
- Litestream：自动识别 `*.digitaloceanspaces.com`，`sign-payload: true`。区域表有 Singapore 1 `sgp1.digitaloceanspaces.com`。https://litestream.io/guides/s3-compatible/ 与 https://litestream.io/reference/config/ 这是文档支持，不是本次对 SGP1 的实测复制。

**Vultr 新加坡**

- 对象存储 FAQ 把地点和主机名写成 Singapore：`sgp1.vultrobjects.com`。同页写 Yes, Vultr Object Storage subscription is S3-compatible。https://docs.vultr.com/products/cloud-storage/object-storage/faq 产品页也把 Singapore SG 列在存储枢纽里。https://www.vultr.com/products/object-storage/ 两页的城市名单不完全相同，以 FAQ 的主机名表为准：新加坡在表内。没有查到「对象只存在该主机名对应城市」的合同原句。
- 兼容矩阵：Put Object、Get Object、Multipart Uploads、Delete Object 为 Yes；Bucket Lifecycle 为 No。https://docs.vultr.com/products/cloud-storage/object-storage/s3-compatibility-matrix （Updated on 10 September, 2025；查询 2026-11-12）不能把删除交给桶的生命周期规则。
- 价目页：Standard Object Storage 展示为 $18/月，含 1 TB 存储和 1 TB 带宽；额外存储 $18/TB（合 $0.018/GB），额外传输 $10/TB（合 $0.01/GB）。https://www.vultr.com/pricing 同页 FAQ：部分数据中心的小时价和月价可能不同。新加坡是否同价：未核实。美元。
- 块存储：价目页展示 NVMe 10 GB 为 $1.00/月。文档有创建块存储快照的 API。https://docs.vultr.com/products/storage/block-storage 快照的新加坡单价，以及快照字节是否留在新加坡：未核实。
- 实例快照 $0.05/GB/月，按压缩后大小。Vultr Snapshots span across all regions。https://docs.vultr.com/products/storage/snapshots/faq （last_updated 2026-10-23）这不是钉在新加坡的副本。
- BGP 社区表把 Singapore, SG 列为 POP。https://docs.vultr.com 的 ASN 说明 PDF（页眉 2026）只证明有这个接入点，不证明块存储或对象存储的字节位置。
- Litestream 的自动识别表和配置参考里没有 Vultr。用通用 S3 endpoint 能否复制：未核实，本次未实测。

**Akamai / Linode 新加坡**

- 对象存储概述原句：Amazon S3-compatible method，并再次写 Amazon S3-compatible storage solution。https://techdocs.akamai.com/cloud-computing/docs/object-storage
- 端点页：Singapore 的区域 ID 是 `ap-south`，标在 E0；Singapore 2 的 ID 是 `sg-sin-2`。2025-01-13 变更写 Object Storage E2 endpoints are now Generally Available (GA) in Melbourne and Singapore。https://techdocs.akamai.com/cloud-computing/docs/object-storage-limited-availability 与 https://techdocs.akamai.com/cloud-computing/changelog/jan-13-2025-object-storage `sg-sin-2` 属于哪一档端点，本次表格摘录不完整，不写死。
- 没有查到「桶内字节不会离开所建区域」的原句。
- 亚太价目页写可用地包含 Singapore。块存储 10 GB 为 $1.00/月，表上按 10 GB 一档翻倍，合 $0.10/GB/月。对象存储一行是 $0.02/GB，出站 $0.005/GB，每月前 1 TB 出站免费。https://www.akamai.com/cloud/pricing/asia-pacific （页上日期 2026-06-24；查询 2026-11-12）对象存储说明另写 $5/月含 250 GB，超出默认 $0.02/GB，并写超额可能因数据中心而不同。https://www.akamai.com/cloud/pricing/object-storage 雅加达和圣保罗被点名为不同的超额；新加坡没有被点名。当前不按请求收费，并写按请求收费不会早于 2027-10-01 出现在账单上。https://techdocs.akamai.com/cloud-computing/docs/object-storage-pricing 美元。共享 CPU 入门机的新加坡月价：未核实。
- 块存储在全部 core compute 区域可用，不在 distributed 区域。最小 10 GB，最大 16 TB，NVMe。卷和 Linode 必须在同一数据中心。https://techdocs.akamai.com/cloud-computing/docs/block-storage Backups 服务不备份块存储卷，即便卷已经挂上。可以克隆卷。https://techdocs.akamai.com/cloud-computing/docs/faqs-for-the-backup-service 实例备份存在同一数据中心的另一套硬件上。卷本身没有快照产品。克隆是否留在同一数据中心：未核实。
- Litestream 文档没有列出 Akamai 或 Linode。未实测。

**阿里云国际站，新加坡**

- OSS 地域表：Singapore，区域 ID `ap-southeast-1`，公网 `oss-ap-southeast-1.aliyuncs.com`，内网 `oss-ap-southeast-1-internal.aliyuncs.com`。https://www.alibabacloud.com/help/en/oss/user-guide/regions-and-endpoints
- 创建桶：Region determines the physical location of your data。同页把合规写成选区域的第一条，并写 ECS 与 OSS 同地域可走内网。核心设置创建后不能改。https://www.alibabacloud.com/help/en/oss/user-guide/create-a-bucket-4 已有桶不能改区域。https://www.alibabacloud.com/help/en/oss/user-guide/0015-00000227
- 标准存储在新加坡可以用同城冗余：多个可用区、同一地域。https://www.alibabacloud.com/help/en/oss/user-guide/overview-of-storage-redundancy-types 与 https://www.alibabacloud.com/help/en/oss/user-guide/overview-53/
- S3 兼容：OSS 支持 Signature Version 4。Python boto3 必须用 Signature V2，因为该客户端的 V4 与分块编码绑在一起。Go v2 的 Manager API 对大文件可能使用分块编码。https://www.alibabacloud.com/help/en/oss/developer-reference/use-aws-sdks-to-access-oss
- Litestream 的受支持路径不是这条 S3 层。配置参考：v0.5.3 起有原生 `type: oss`，用官方 SDK。示例 URL 是 `oss://mybucket.oss-cn-hangzhou.aliyuncs.com/db`，区域例子是 `cn-hangzhou`、`us-west-1`。https://litestream.io/reference/config/ 新加坡区域 ID 能否填进 `region`：未核实，本次未实测。
- OSS 价目页列出 Singapore (Singapore)，并展示标准存储（本地冗余）超过 5 GB 为 US$0.0173、同城冗余 US$0.0232。同一页先列出青岛、北京等一串地域名。本次抓取不能确认这些数字属于新加坡而不是默认地域。新加坡 OSS 单价：未核实。https://www.alibabacloud.com/en/product/oss-pricing-list/en
- 云盘按容量计费，创建后即计费，与是否挂载无关；单价可能因地域不同，文档让读者去 ECS 定价页。快照示例里的 US$0.0173/GB-月写的是中国（杭州），不是新加坡。https://www.alibabacloud.com/help/zh/ecs/block-storage-devices 与 https://www.alibabacloud.com/help/doc-detail/56159.htm 新加坡云盘单价和新加坡快照单价：未核实。ECS 各规格是否在新加坡在售：本次没有打开 ECS 地域清单，未核实。

**腾讯云国际站，新加坡**

- 国际站文档：地域是托管机房的分布地区，对象存储的数据存放在这些地域的存储桶中。新加坡地域简称 `ap-singapore`，默认域名 `<BucketName-APPID>.cos.ap-singapore.myqcloud.com`。最后更新时间 2026-10-13。https://intl.cloud.tencent.com/zh/document/product/436/6224
- 多 AZ 当前包括新加坡，数据分散在同一城市的多个数据中心。https://intl.cloud.tencent.com/zh/document/product/436/35208 （最后更新 2026-05-29）
- S3：COS provides an S3-compatible implementation，并写 COS does not guarantee full compatibility with S3。另一页写 COS 提供 S3 兼容 API，示例区域是 `ap-guangzhou`。https://intl.cloud.tencent.com/document/product/436/34688 与 https://intl.cloud.tencent.com/document/product/436/32537
- Litestream 的提供商表没有腾讯云 COS。未实测。
- 云硬盘价目页有 Southeast Asia (Singapore) 一行。快照表写明该地域 Snapshot capacity 为 $0.000036 / GiB-小时，折合 $0.026 / GiB-月。https://buy.tencentcloud.com/pricing/cbs 云硬盘各类型的列在本次摘录里没有列名，不把同一行里的 0.05、0.11、0.18、0.22 安到某一种盘上。分类型单价：未核实。美元。
- COS 按量价目页展示 STANDARD 存储使用费 0.016 USD/GB/月，标准读写请求各 0.002 USD/万次。https://buy.tencentcloud.com/pricing/cos 摘录没有写这一列是不是新加坡。新加坡对象存储单价：未核实。
- CVM 新加坡的实例月价：未核实。

**Google Cloud，`asia-southeast1`**

- 位置表把 `ASIA-SOUTHEAST1` 写成 Singapore。Cloud Storage stores object data in the selected location in accordance with the Service Specific Terms。区域位置是同步的可用区冗余，读同一区域的数据没有出站费。https://docs.cloud.google.com/storage/docs/locations （页面时间 2026-11-03）服务条款正文没有通读。
- 持久盘和快照产品存在。快照可以放在区域位置，例如文档列举的 `asia-south1`。源盘若位于某个多区域所覆盖的位置，项目的默认快照位置是该多区域；举例 `us-central1` 落到 `us`，`australia-southeast1` 落到 `asia`。可以改成与源盘相同的区域。https://docs.cloud.google.com/compute/docs/disks/snapshot-settings 与 https://docs.cloud.google.com/compute/docs/disks/snapshots （后者页面时间 2026-11-10）`asia-southeast1` 的默认快照是不是只在新加坡：未核实。不改默认值就不能把快照当成新加坡副本。
- Litestream 对 GCS 的支持方式是原生类型，不是 S3 副本。配置参考：`type: gs` 或 `url: gs://...`，认证用 Application Default Credentials（v0.5.0 起的副本类型列表含 `gs`）。https://litestream.io/reference/config/ 另有 XML API，可换端点 `https://storage.googleapis.com` 并用 HMAC 键，给一部分 S3 工具用。https://docs.cloud.google.com/storage/docs/interoperability Litestream 是否接受用这条 XML 端点配 `type: s3`：未核实。因此 GCS 不满足「S3 兼容且按 Litestream 的 S3 路径使用」。
- 存储价目页按区域展开。本次抓取在列出 Singapore (`asia-southeast1`) 的同时看到标准存储 $0.000027397 / GiB-小时。页面按区域切换，摘录没有标明这个数字属于新加坡。新加坡标准存储单价：未核实。https://cloud.google.com/storage/pricing 持久盘的新加坡单价：未核实。
- 本次没有用 GCS 做复制。

**Hetzner**

- 地点表有新加坡 `sin`（网络区 `ap-southeast`）。新加坡可用：Cloud Shared AMD、Cloud Dedicated AMD、卷、服务器快照、服务器备份、防火墙、网络、负载均衡。不可用：Object Storage（只在法尔肯施泰因、纽伦堡、赫尔辛基打勾）。https://docs.hetzner.com/cloud/general/locations/
- 对象存储产品页也写目前只在 FSN1、HEL1、NBG1。基础价 €6.49/月（不含 VAT），含 1 TB 存储和 1 TB 出站。https://www.hetzner.com/storage/object-storage/
- 卷：10 GB 到 10 TB，每块数据存在三台物理机上。官方写明不为卷提供备份或快照；服务器的备份和快照也不包含挂上的卷。https://docs.hetzner.com/cloud/volumes/overview/ 与 https://www.hetzner.com/cloud/block-storage/
- 卷的每 GB 单价，本次打开的页面没有给出数字。未核实。
- 2024-08-06 的新闻写过新加坡入门机型月价。那是旧闻，不引用为现价。https://www.hetzner.com/news/new-location-singapore/
- Litestream 对 Hetzner 对象存储的 endpoint 探测不能改变「桶不在新加坡」这件事。

### 2.5 和合规研究对齐的地方

备份是处理，不是「只是一份拷贝」。对齐 `compute-service-compliance-research.md`，不重复论证法条：

- 新加坡是部署地点，不是泰国 PDPC 的充分性目的地。研究第 2 节：充分性名单未公布，新加坡和美国都不能填成已获充分性。把副本再送到美国或欧盟，是另一跳，要沿用同一节的「适当保障 / 合同」，不能靠「这只是备份」。
- 研究第 2 节引用的 PDPC AI 指引摘要：境外若有发送方以外的人能访问，可能构成跨境；单纯无人能访问的存储，指引认为可以不是传输。对象存储的密钥、控制台账号、支持工单都能访问时，按「可能是传输」设计。本次没有为某一家云核实「供应商员工能否读对象」。
- 研究第 6 节：新加坡 PDPA 第 26 条的传出义务，在数据中介路径下不一定直接落在中介身上，但一旦 idoris 被看成为自己的目的而处理，第 26 条会直接落到它身上。备份桶的区域是这个问题的输入，不是结论。
- 研究第 6 节：保存限制。账本里的假名 id 若算个人数据，要有目的和停用后的删除期限。副本的保留时间不能长过这个期限，除非另有目的并写进告知。
- 研究第 1 节和第 6 节：假名成员 id 仍可能是个人数据。备份里有它，就按个人数据保护，不能说「没有姓名就不是」。
- 设计稿 §7：正文只在内存里处理，edu 关缓存。研究要求日志和错误报告也不写儿童姓名、学校、班级。备份扫描要覆盖这些，不能只扫事件日志。

律师还没回答的、和备份直接相关的问题，仍是研究第 2 节和第 6 节末尾那些：用户设备到新加坡算不算第 28 条、新加坡再向外的合同文本、假名 id 在新加坡法下算不算个人数据、泄露时谁通知。本文不替他们回答。

## 3. 现有代码里的 SQLite，以及建议改什么

本次没有改这些文件。行号是 2026-11-12 本工作区。

四个 store 打开同一个文件（`crates/idoris-router/src/storage.rs:190-198`）：

| 连接 | 打开 | 进程内互斥 | 显式 PRAGMA |
|---|---|---|---|
| `TenantStore` | `crates/idoris-tenancy/src/store.rs:65-66` | 外层 `Arc<Mutex<_>>`（`storage.rs:239`） | 无 |
| `BudgetLedger` | `crates/idoris-tenancy/src/budget/ledger.rs:296-298` | 内部 `Mutex<Connection>`（`ledger.rs:241`） | `journal_mode=WAL`，`busy_timeout` 5 秒 |
| `VirtualKeyStore` | `crates/idoris-tenancy/src/virtual_key/store.rs:44-45` | 外层 `Arc<Mutex<_>>`（`storage.rs:241`） | 无 |
| `EventLogStore` | `crates/idoris-tenancy/src/event_log/mod.rs:104-105` | 内部 `Mutex<Connection>`（`mod.rs:101`） | 无 |

另外，进程启动前用 `<db>.lock` 做 OS 排他锁，崩溃由内核放开（`storage.rs:101-135`）。所以 P0 的「一个活动写进程」已经有一把锁。锁不阻止这一个进程里的四条连接轮流写，也不阻止 Litestream 再开一条连接。

rusqlite 0.40.2（`Cargo.lock` 里的版本）在 `Connection::open` 里调用 `sqlite3_busy_timeout(db, 5000)`（crates.io 上该版本的 `src/inner_connection.rs:118`）。仓库里的注释也这么写（`virtual_key/store.rs:311-312`）。所以另外三条连接今天会忙等 5 秒，但是靠依赖的默认值，不是自己设的。Litestream 要求的就是这 5 秒。

`synchronous` 和 `wal_autocheckpoint` 在 `crates/` 里没有设置。SQLite 默认是 `FULL` 和 1000 页自动 checkpoint（第 2.3 节）。

账本注释写明：每次写在 `BEGIN IMMEDIATE` 里，WAL 加 5 秒忙等，用来堵住先检查后扣款（`ledger.rs:231-239`）。这只覆盖账本自己的连接。用量、事件、虚拟 key 是另外三条连接，不在预算事务里（勘察文第 2 节末尾已经写了）。

`crates/` 里没有 `VACUUM`。

### 建议的 PRAGMA（给以后的代码改动，本次不改）

每一条业务连接在打开后执行，并检查返回值，不要只调用然后丢掉结果：

```sql
PRAGMA journal_mode = WAL;          -- 必须返回 wal
PRAGMA busy_timeout = 5000;         -- 每条连接都写上，不依赖 rusqlite 默认
PRAGMA synchronous = FULL;          -- 显式钉住现状
```

`journal_mode` 是库文件的属性，设一次就留下来。今天只有账本连接去设（`ledger.rs:297`），而且没有检查返回值是不是 `wal`。启动顺序是先打开租户库再打开账本。空闲的另一条连接不一定挡住切换；有未提交事务时，另一次 `PRAGMA journal_mode=WAL` 会得到 `database is locked`（2026-11-12 用本机 Python 的 sqlite3 模块试过，不是用 idoris 进程试的）。建议：四条连接都断言当前模式是 `wal`；账本那次若返回的不是 `wal`，启动失败。

**不要在 P0 把 `wal_autocheckpoint` 设成 0。** Litestream 在很高的写入下才建议关掉应用自己的 checkpoint。关掉之后，Litestream 停了而进程还在写，WAL 会一直涨，直到磁盘满。一个班的流量远没到文档里的「每秒数万事务」，先保持 SQLite 默认的 1000 页。压测若看到 Litestream 不停地做全量快照，再改成 0，并且同时告警「Litestream 进程不在」。

**`synchronous` 保持 FULL。** Litestream 建议 `NORMAL` 是为了少 fsync。SQLite 写明 `NORMAL` 在 WAL 下断电可能回滚已提交事务，丢失窗口是到下一次 checkpoint，不一定小于复制间隔。账本要的 RPO 是「已提交的结算最多丢掉复制间隔」，不是「再丢掉未 fsync 的 WAL」。等压测证明 FULL 的延迟过不了线，再由 owner 改 NORMAL。

**不要开原地 `VACUUM`。** 以后要收缩文件，用 `VACUUM INTO` 换一个新文件，停复制、换路径、再让 Litestream 对新文件做全量快照。见第 2.1 节。

**外键：** 现有迁移没有把完整性交给外键。`foreign_keys` 是每条连接的开关，不持久。在 schema 真正使用外键之前不要为了「最佳实践」单独打开，避免测试和生产不一致。

**单连接：** 不建议为了 Litestream 收成一条连接。四条连接在 WAL 下由 SQLite 串行写。要改的是每条连接都忙等，以及账本的 `BEGIN IMMEDIATE` 继续包住检查和预留。收成一条连接是另一个并发设计，不属于本次运维方案。

## 4. WAL 持续备份与恢复

### 4.1 运行形态

- 一个写进程拿着 `<db>.lock`。Litestream 作为旁边的进程，只复制这一个库文件。
- 副本 URL 唯一。禁止第二个 Litestream 写同一路径。
- 库文件、`-wal`、`-shm` 都在持久卷上。复制的是逻辑内容（LTX），不是把 `-shm` 当备份文件上传。
- 配置放在卷外的只读配置里。密钥用环境变量，不写进仓库。
- `sync-interval: 10s`。默认 1 秒已经满足 1 分钟，10 秒是为了留出上传失败的重试，仍低于 60 秒。若连续失败，告警看的是「距上次成功同步」，不是配置值。
- `snapshot.interval: 1h`，`snapshot.retention` 取 owner 定的账本保留期，并且不短于演练周期。恢复时间大致是「拉最近快照 + 重放之后的 LTX」。1 小时快照是为了把 RTO 留在 30 分钟以内；上线后用真实库的体积把恢复计时，超过 20 分钟就把快照再加勤。
- `validation.interval: 15m`，用来发现 LTX 空隙。恢复演练仍要跑 `integrity_check`。
- `l0-retention` 至少 24h，这样一天内的恢复点不至于被压实。更长的保留由 owner 按保存期限定。
- 关闭超时保持默认 30 秒。部署脚本先给 Litestream SIGTERM，等它退出，再停应用；或者反过来先停应用再停 Litestream，但两种顺序都要在演练里做过。推荐：先停应用（不再写），再 SIGTERM Litestream，让最后一次同步把已提交的 WAL 带走。
- 磁盘水位：卷要能装下主文件、WAL 高峰和 Litestream 本地元数据（`.<filename>-litestream`）。WAL 超过卷的 20% 就告警。

建议配置的形状（密钥用环境变量，不要提交真实值）：

```yaml
snapshot:
  interval: 1h
  retention: 168h
l0-retention: 24h
validation:
  interval: 15m
logging:
  level: info
  type: json
dbs:
  - path: /var/lib/idoris/idoris.sqlite3
    replica:
      type: s3
      bucket: idoris-ledger-sin
      path: prod/idoris.sqlite3
      endpoint: sgp1.digitaloceanspaces.com
      region: sgp1
      sync-interval: 10s
```

这是首选 Spaces 的形状，键名按 Litestream 的 Spaces 指南。https://litestream.io/guides/s3-compatible/ 与 https://litestream.io/reference/config/ 不要打开这个桶的 CDN。

备选 Tigris 必须是创建时就限定为单区域 `sin` 的桶。Litestream 侧的端点是 `fly.storage.tigris.dev`，region 填 `auto`。指南里的 URL 形状是 `s3://BUCKETNAME?endpoint=fly.storage.tigris.dev&region=auto`。https://litestream.io/guides/tigris/ 这条 URL 本身不选择新加坡；区域是桶的位置，不是这个 `region=auto`。

### 4.2 恢复步骤

目标是 30 分钟内让一个新的单写节点用恢复出的库提供服务。计时从「决定恢复」开始，到「新进程拿锁并通过对账前检查」为止。

1. 确认旧进程已经不在。旧机器若还活着，先停应用和 Litestream，避免两个写者。`<db>.lock` 只在同一台机器上有效，换机器时锁不起作用。
2. 在新卷上准备空目录。不要把旧的 `-wal`、`-shm` 或 `.<file>-litestream` 拷进去。
3. `litestream restore -o /var/lib/idoris/idoris.sqlite3 s3://idoris-ledger-sin.sgp1.digitaloceanspaces.com/prod/idoris.sqlite3`。需要回到某个时间点时加 `-timestamp`。时间用 UTC。备选 Tigris 用它自己的 endpoint 查询参数，不要把 Spaces 的 URL 拿去恢复 Tigris 桶。
4. 打开恢复出的文件，执行 `PRAGMA integrity_check;`，结果必须是单独一行 `ok`。然后 `PRAGMA journal_mode;` 必须是 `wal`。
5. 扫描库里不应出现的正文键（第 6 节的扫描）。有命中就不对外服务，先隔离这份库。
6. 跑第 7 节的对账。对账完成前，入口保持拒绝新的付费和补贴请求。只读查询可以开，写入不开。
7. 启动新的 idoris，确认它拿到 `<db>.lock`。然后启动一个新的 Litestream，指向新的副本路径或确认旧路径只有这一个写者。不要在旧进程还在复制时启动第二个。
8. 记下恢复开始、restore 结束、integrity_check 结束、对账结束四个时间。RTO 用最后一笔。

卷快照只在对象存储副本也坏了的时候用，并且接受它的 RPO 是「上一次快照」，通常是一天，不满足 1 分钟。用快照拉起之后，仍然做 integrity_check 和对账。

### 4.3 MinIO 步骤（未在本机执行）

2026-11-12 执行 `docker pull minio/minio` 返回 `pull access denied ... repository does not exist or may require docker login`。`quay.io/minio/minio` 返回 401。因此没有提交依赖 MinIO 的脚本。有镜像之后，步骤是：

1. 只在本机 Docker 网络起 MinIO，访问键用 `minioadmin` / `minioadmin`，桶名自定。不要把这个默认键用于任何真实桶。
2. Litestream 副本 URL 指向 `http://minio:9000`，`force-path-style: true`，region 按文档可填 `us-east-1`（MinIO 忽略该值）。https://litestream.io/guides/s3-compatible/
3. 写入一行，`replicate -once` 或让 replicate 跑过一个 sync 间隔，再写入一行，再同步一次。
4. 删掉本地库、`-wal`、`-shm`，`restore -o` 到新路径。
5. `integrity_check` 为 `ok`，两行都在；同步之后的第三行不在。

第 5 节的文件副本演练就是同一套断言，只是副本在本地目录。

## 5. 恢复演练

### 5.1 每次演练做什么

频率建议每 30 天一次，外加每次改复制配置之后。用生产副本的一份拷贝，或者用演练桶。不要把演练恢复写回生产路径。

1. 记录开始时间、副本里最新 LTX 的时间、库文件大小。
2. 恢复到一个空目录。
3. `PRAGMA integrity_check` 为 `ok`。
4. 正文键扫描为 0 行（第 6 节）。
5. 用第 7 节的只读对账跑一遍生产形状的数据：重复结算为 0，漏记进入待补清单而不是自动向用户再收一次。
6. 记录 restore 耗时。超过 20 分钟则把快照间隔缩短，并在下一次演练复查。
7. 演练库用完删除。演练卷若含假名 id，按生产备份同样的访问控制和保留期限处理。

通过线：integrity_check 为 `ok`；演练在 30 分钟内做完第 2 到第 5 步；没有正文键命中；对账分类和预期一致。做不到就没有通过，不能用「脚本退出码是 0」代替生产演练。

### 5.2 本机脚本

`scripts/compute-ops/rehearse-litestream-restore.sh`

它使用本机已经有的镜像 `litestream/litestream:0.5.18`，`docker run --pull never`，副本是 `file://`。工作目录在脚本旁边的 `.drill-work`，结束时删除。需要本机 `sqlite3` 和 Docker。没有镜像就退出，不会拉取。

断言：

- 复制过的行 A、B 在恢复结果里。
- 最后一次复制之后写入的 C 不在。这是 RPO 窗口，不是失败。
- `integrity_check` 为 `ok`。
- 对账夹具的分类与第 7 节一致：已结算、重复结算、漏记、保持预留。
- 同一条幂等键的第二次插入被 SQLite 拒绝。

2026-11-12 在本工作区执行通过。输出见文末附录。这只证明工具链和分类夹具，不证明生产桶、网络或 30 分钟 RTO。

## 6. 备份里有什么

设计稿 §7：正文只在内存里处理；edu 关响应缓存；幂等记录只存请求摘要和结算状态。反向代理、错误采样、崩溃转储、安全分类器、Modal 日志都在不落盘的范围内。

当前代码里，SQLite 实际会有的内容（都会进 WAL，因此都会进副本）：

- 账本：租户 id、限额、已花费的整数金额、预留 id、状态、时间。没有 prompt 列。表形状见勘察文 §1.1，`crates/idoris-tenancy/src/budget/migrations/`。
- 用量记录：`ts_utc`、`cost_minor`、`tokens_in`、`tokens_out`、`request_id`。写入在 `crates/idoris-tenancy/src/usage.rs:39-60`。
- 事件日志：白名单字段，包括供应商、模型、token、金额、原因码。明确拒绝 `prompt`、`messages`、`content`、`body` 等正文键（`crates/idoris-tenancy/src/event_log/mod.rs:10-13`）。触发器禁止普通 UPDATE/DELETE，但能改文件的人可以拆掉触发器（`event_log/mod.rs:93-100`）。备份能恢复这个日志，不能把日志变成防篡改。
- 虚拟 key：`key_id`、SHA-256、`owner`、可选 `tenant_id`、隐私和角色的 JSON、过期时间、状态。明文密钥不入库（`virtual_key/store.rs:53-54` 一带的注释，以及签发路径）。哈希和 owner 字符串仍然在副本里。
- 租户 id、请求 id、预留 id。合规研究认为假名 id 仍可能识别到孩子。

当前代码里不应在 SQLite 中的：

- 聊天正文。代理缓存是进程内内存，重启即空（勘察文约第 350 行）。本次没有再打开那一段，沿用勘察。
- 设计稿要求的尝试表。仓库里还没有 `attempt` 这种表。恢复后对账的目标形状在第 7 节，今天的 `reservations.status` 是可更新的，不是不可变分录。

副本同样会有的风险：

- 谁以后把正文写进 `tenant_records.payload` 或事件 metadata，Litestream 会照样复制。所以演练要扫，不能只靠代码审查。
- 崩溃转储和反向代理日志不在这个 SQLite 里。它们有自己的不落盘要求，本文的副本覆盖不到。上线验收仍要扫那条链路（设计稿 §7、§13）。
- `-shm` 不进副本。恢复出来的是一致的库文件，不是热 WAL 的半页。

建议的扫描（演练和第 4.2 步第 5 步）：

```sql
-- 事件 metadata 与用量 payload 里不该出现的键。有行就停止上线。
SELECT 'event_log' AS src, event_id
FROM event_log_events
WHERE lower(metadata) LIKE '%"prompt"%'
   OR lower(metadata) LIKE '%"messages"%'
   OR lower(metadata) LIKE '%"content"%'
   OR lower(metadata) LIKE '%"body"%';

SELECT 'usage' AS src, record_id
FROM tenant_records
WHERE lower(payload) LIKE '%"prompt"%'
   OR lower(payload) LIKE '%"messages"%'
   OR lower(payload) LIKE '%"content"%';
```

这是粗扫描。JSON 键中间有空格时会漏。上线前改成按 JSON 抽取键名，和 `CONTENT_KEYS`（`event_log/mod.rs:13`）同一份清单。学校、班级、儿童姓名若出现在任何文本列里，同样算命中，即使键名不在清单里。

访问控制：副本桶不公开，密钥只给 Litestream 和恢复角色。能读副本的人能读假名 id 和金额。按个人数据做最小权限。保留到期后删除对象；若 Litestream 自己删，凭证需要删除权限，泄露面更大。可以关掉 Litestream 的远端删除，改由桶的生命周期规则删除（配置参考里的 `retention.enabled: false`）。选哪一种由 owner 定。

## 7. 恢复后对账

设计稿 §6 和 §10：尝试有 `maybe_executed` 和 `executed`；用户只为最终给出回答的那次尝试付钱；其他尝试的成本由平台承担；`maybe_executed` 按预留额挂着，等账单或 usage，不马上释放。§9 的组织对账用尝试 ID 和不可变账目做幂等游标。

今天没有尝试表，也没有不可变分录。`reservations.status` 会从 `active` 改成 `settled` / `released` / `expired`（勘察文 §1.1）。下面的算法是 S4 之后的目标。在那张表落地之前，不能用「把 status 再改一次」来实现它。恢复后如果仍是今天的 schema：不要自动把 `settled` 再结算一遍；`active` 且已过期的预留走现有的过期清扫；`maybe_executed` 还不存在，无从核对。把这一点当作上线缺口，而不是用现有 status 字段假装已经对过账。

### 7.1 目标数据

- `attempts`：一行一个尝试，主键 `attempt_id`。字段至少有 `request_id`、`state`（`not_sent` / `maybe_executed` / `executed` / `settled`）、`pays_user`（这次是不是向用户收费的那一次）、`supplier`、`supplier_request_id`、`price_version`、`reserved_minor`、usage。尝试行一旦写下就不再改状态以外的身份字段；状态迁移另说。更稳的做法是状态也做成只追加的事件，但 P0 最少要保证分录不可变。
- `postings`：只追加。列：`posting_id`、`attempt_id`、`kind`（`user_price` / `platform_cost` / `hold` / `release`）、`amount_minor`、`idempotency_key`。`idempotency_key` 唯一。禁止 UPDATE 和 DELETE。冲正是一条新的负数分录，带新的 id 和新的键。
- `upstream`：供应商账单行或「明确查无此单」的证据。没有拉到账单文件，不等于查无此单。

用户价只记在 `pays_user = 1` 的那次尝试上。其余尝试只记 `platform_cost`。

### 7.2 伪代码

```text
required_kind(attempt):
  if attempt.pays_user == 1: return "user_price"
  else: return "platform_cost"

net(attempt_id, kind):
  return sum(postings where attempt_id and kind)   # 含冲正

classify(attempt, upstream_row, now):
  posted = net(attempt.attempt_id, required_kind(attempt))

  # 同一请求上，向用户收了不止一次
  if count(attempts of attempt.request_id where pays_user == 0
           and net(attempt_id, "user_price") != 0) > 0
     and attempt.pays_user == 0:
    return DUPLICATE          # 平台尝试被记成了用户价

  if count(postings of this attempt where kind == "user_price" and amount > 0) > 1
     and attempt.pays_user == 1:
    return DUPLICATE          # 赢的那次被记了两次；幂等键正常时插不进去

  if upstream_row.evidence == "line"
     and attempt.state in ("maybe_executed", "executed")
     and posted == 0:
    return MISSING            # 漏记

  if attempt.state == "maybe_executed" and upstream_row is absent:
    return HOLD               # 没有证据，继续挂预留

  if attempt.state == "maybe_executed"
     and upstream_row.evidence == "not_found"
     and now < attempt.hold_until:
    return HOLD               # 查无此单也要等到持有期结束

  if attempt.state == "settled" and posted != 0 and upstream_row.evidence == "line":
    return SETTLED

  if attempt.state == "settled" and upstream_row is absent:
    return UPSTREAM_LATER     # 不要自动冲正，账单可能还没到

  return REVIEW

apply(item):
  if item is DUPLICATE:
    # 不删除原分录。若第二次已经写入，追加一条冲正到平台，并告警。
    # 幂等键在插入时就应该拒绝第二次。拒绝本身就是处理。
    alert("duplicate_settlement", item.attempt_id)
    do not charge the user again

  if item is MISSING:
    key = attempt_id + "|" + required_kind + "|settle|" + upstream.bill_id
    insert posting(kind = required_kind, amount = billed or reserved, key)
      on unique conflict: stop, it was already applied
    # pays_user == 0 时 kind 是 platform_cost，不要写成 user_price

  if item is HOLD:
    do nothing

  if item is UPSTREAM_LATER:
    alert("upstream_missing", item.attempt_id)
    do not reverse

  if item is SETTLED:
    do nothing
```

恢复时尚不存在本地尝试行、但账单上有供应商请求号：这是整行丢失，不是第二次结算。不要发明 `attempt_id`。标成 `orphan_upstream_charge`，人工用供应商请求号去对。对上之后补尝试行和恰好一条分录，键用供应商请求号，避免下次演练再补一次。

`not_sent` 的尝试如果账单上却有扣款，按漏记处理，并告警「状态与账单矛盾」。不要把它自动改成用户价，除非发布记录证明这次回答给了用户。

### 7.3 哪些情况算什么

| 情况 | 分类 | 处理 |
|---|---|---|
| 同一 `idempotency_key` 再插入 | 重复结算 | 数据库拒绝。不重试成另一条键 |
| 一次请求里有两条正的 `user_price` | 重复结算 | 保留赢的那次；多出来的追加冲正到平台。不向用户再收 |
| `maybe_executed` 或 `executed`，账单有行，对应 kind 的净额为 0 | 漏记 | 追加恰好一条分录。用户尝试用 `user_price`，其余用 `platform_cost` |
| `maybe_executed`，没有账单文件 | 保持预留 | 不释放，不记 0 |
| 供应商明确查无此单，持有期未到 | 保持预留 | 不释放 |
| 供应商明确查无此单，持有期已过 | 可以释放 | 追加 `release`，键含证据 id。持有期由 owner 定，本文不定天数 |
| 本地已结算，账单还没有 | 账单延迟 | 只告警，不冲正 |
| 账单有、本地完全没有尝试行 | 孤儿扣款 | 人工对供应商请求号。不要自动生成用户账单 |
| 恢复丢掉的最后几十秒 | 可能漏记或孤儿 | 用账单补。不能靠把旧分录再放一遍 |

脚本里的夹具覆盖了：已结算、重复的用户价、没有账单的预留、有账单但没分录的用户尝试、有账单但没分录的平台尝试，以及幂等键拒绝第二次插入。

## 8. 压测计划

设计稿写明 QPS 还不知道。下面是测法，不是测过的数字。通过线里的秒数是建议值，owner 可以改，但不能不写就上线。

环境：与生产相同的单写进程、同一条持久卷类型、Litestream 复制开着、`synchronous=FULL`、四条连接的现状。先在恢复演练用的小库上把工具跑通，再换接近上线的数据量。不要对生产库压。

先测出容量，而不是猜 QPS：

1. 准备一个租户、一笔足够大的额度，避免 402 把锁的问题掩盖掉。
2. 并发从 1、5、10、20、40、80 往上加。每一档持续 10 分钟。40 只是「一个班」的起点，真实班额由 owner 填进报告。
3. 每一档记录：成功请求数、每秒成功数、p50 和 p99 延迟、HTTP 500 里属于账本忙的次数、`BudgetError::Busy`（勘察：`SQLITE_BUSY` 映射成这个错误，路由把它变成 500，不是 402）、WAL 文件大小、卷剩余空间、Litestream 距上次成功同步的秒数。
4. 容量取最后一档仍同时满足：p99 < 2 秒、忙等导致的失败 < 1%、复制滞后 p99 < 30 秒、卷剩余 > 20%、`integrity_check` 仍为 `ok`。更高一档若打破其中任何一条，前一档就是当前配置的上限。这个上限就是以后告警用的 QPS，而不是上线前写死的数。

场景：

| 场景 | 方法 | 建议通过线 |
|---|---|---|
| 一个班同时发请求 | 上面的 40 并发档，或 owner 给的班额。请求打到会走预留和结算的接口 | 该档落在容量以内。没有超发：结算额不超过限额 |
| 账本锁等待 | 一条连接拿着 `BEGIN IMMEDIATE` 不提交，另一条做预留，测量忙等。然后放开 | 5 秒内要么成功要么返回忙，不挂死。忙返回 500 的行为要在报告里写明，直到产品改成别的状态码。锁放开后下一次预留成功 |
| 磁盘写满 | 把卷填到只剩几 MB，再写预留。用独立卷，不要填笔记本系统盘 | 写入失败并返回错误。已提交的行还在。`integrity_check` 为 `ok`。Litestream 不删除还没复制的本地 WAL。腾出空间后复制能追上，滞后回到 30 秒内 |
| 进程崩溃 | 写到一半 `kill -9` 应用，Litestream 仍在；再 `kill -9` 两者 | 应用崩溃后，已提交事务都在，复制最终跟上。两者一起被杀死时，丢掉的只有上次成功同步之后的事务，用副本恢复后对账能把账单上有、本地没有的标成漏记或孤儿 |
| 恢复后重复结算 | 恢复到演练库，把同一批 `settled` 尝试再送一遍结算 | 第二次被幂等键拒绝或分类为重复。用户净额不变。平台不多收 |

磁盘写满和崩溃都要在 Litestream 开着的时候做一次。只测应用、不测复制，看不出 RPO。

## 9. 告警

每条都要有接收人。没有接收人就不算告警。阈值里的 30 秒对应 RPO 60 秒的一半，用来在破线之前响。

| 条件 | 为什么 |
|---|---|
| Litestream 进程不在，超过 1 分钟 | 复制停了，RPO 开始随时间变大；若以后关掉 autocheckpoint，WAL 还会涨 |
| 距上次成功同步 > 30 秒，持续 2 分钟 | 已经吃掉一半 RPO |
| 验证任务报告 LTX 空隙或重叠 | 副本可能恢复不出来 |
| 卷剩余 < 20%，以及 < 10% | 10% 时停止接收新的付费请求，避免写满后的不确定状态 |
| WAL 文件 > 卷容量的 20% | checkpoint 卡住或复制停了 |
| 账本忙等失败在 5 分钟内 > 1% 的请求 | 到了第 8 节的容量线 |
| 演练超过 30 天没做，或最近一次失败 | RTO 没有证据 |
| 对账出现重复结算、孤儿扣款、或漏记清单在持有期内没处理 | 钱会错 |
| 正文键扫描 > 0 | 备份里出现了不该落盘的内容 |
| 恢复后的 `integrity_check` 不是 `ok` | 不能把这个文件对外 |
| 同一个副本路径出现第二个复制进程 | 文档写明这样会复制坏 |

磁盘、进程和复制滞后要在写满和 kill 的压测里各响一次，证明告警真的接上了。

## 10. 需要 owner 决定的事

1. 计算节点按本文首选放在 DigitalOcean SGP1，还是改用备选 Fly.io `sin`。本文只比较了公开的卷和对象存储能力，没有测延迟。
2. 副本按首选用 Spaces SGP1，还是改用备选 Tigris 单区域 `sin`。不要在没做跨境决定时改用 R2 或 Hetzner 对象存储。Tigris 若用，桶必须在创建时限定为 `sin`，并接受对象键和系统元数据会复制到所有区域。
3. 账本和副本的保留天数。必须和合规研究里的保存限制一致，不能为了恢复方便无限保留假名 id。
4. 持有期：`maybe_executed` 在「查无此单」之后再挂多久才允许 `release`。
5. `synchronous=FULL` 是否在压测后改成 `NORMAL`。默认维持 FULL。
6. 远端对象由谁删除：Litestream 的删除权限，还是桶的生命周期规则。
7. 一个班的并发人数，用来替换压测起点 40。
8. 第 8 节建议通过线（p99 2 秒、忙等失败 1%、复制滞后 30 秒）是否接受。
9. 恢复期间谁有权宣布入口拒绝写入。30 分钟 RTO 包含这个决定的时间。
10. 备份密钥和桶的管理员是否算「发送方以外的人能访问」。这是交给研究第 2 节和第 6 节的律师问题，不是工程默认。

## 11. 本次没有核实或没有做成的

- MinIO 演练没有跑。镜像拉不下来。步骤在第 4.3 节。
- 没有用真实的 Spaces、Tigris、OSS、COS、GCS、Vultr 或 R2 做复制。文件副本演练不能证明云上的延迟和权限。
- DigitalOcean SGP1 的 Droplet 月价、Hetzner 卷的每 GB 单价：未核实。
- Vultr 块存储快照的单价和字节位置、新加坡计算与对象存储是否与标价页同价：未核实。
- Akamai 共享 CPU 入门机的新加坡月价、卷克隆是否留在同一数据中心、`sg-sin-2` 的端点等级：未核实。
- 阿里云新加坡的 OSS 单价、云盘单价、快照单价、ECS 规格是否在售，以及 Litestream `region: ap-southeast-1` 能否工作：未核实。
- 腾讯云新加坡的 COS 按量单价、云硬盘分类型单价、CVM 月价，以及 Litestream 对 COS 的兼容：未核实。
- Google Cloud `asia-southeast1` 的标准存储单价、持久盘单价、默认快照是否只在新加坡，以及 Litestream 用 XML/HMAC 端点走 `type: s3`：未核实。
- Tigris 单区域 `sin` 的灾难恢复区域叫什么、Litestream 上传会不会改掉桶级位置：未核实。
- LiteFS v0.5.14 的发布年份：未核实。
- Litestream PutObject checksum 的配置键名：未核实。
- sqlite3_rsync 在 3.51 是否还包含 3.50.4 的损坏问题：未核实。
- R2 `apac` 提示实际落在哪个数据中心：未核实。
- 供应商员工能否读取对象：未核实。
- 生产 QPS、恢复耗时、一个班的真实并发：没有测量。
- CodeGraph 在本工作区没有索引。行号是直接读源码得到的。

## 附录：2026-11-12 本机演练输出

命令：`./scripts/compute-ops/rehearse-litestream-restore.sh`

镜像：本地已有的 `litestream/litestream:0.5.18`（容器内 `version` 为 `v0.5.18`）。`--pull never`。无密钥。

结果摘要：`integrity_check` 为 `ok`；恢复出 `A|settled` 和 `B|maybe_executed`；C 不在恢复结果中；第二次插入幂等键失败（`UNIQUE constraint failed: postings.idempotency_key`）；分类为 `extra=重复结算,held=保持预留,miss=漏记,plat=漏记,win=已结算`；最后一行 `PASS`。
