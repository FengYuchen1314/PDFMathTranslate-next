<div align="center">

<img src="./docs/images/banner.png" width="320px"  alt="banner"/>

<h2 id="title">PDFMathTranslate</h2>

<p>
  <!-- PyPI -->
  <a href="https://pypi.org/project/pdf2zh-next/">
    <img src="https://img.shields.io/pypi/v/pdf2zh-next"></a>
  <a href="https://pepy.tech/projects/pdf2zh-next">
    <img src="https://static.pepy.tech/badge/pdf2zh-next"></a>
  <a href="https://hub.docker.com/repository/docker/awwaawwa/pdfmathtranslate-next/tags">
    <img src="https://img.shields.io/docker/pulls/awwaawwa/pdfmathtranslate-next"></a>
  <!-- <a href="https://gitcode.com/PDFMathTranslate-next/PDFMathTranslate-next/overview">
    <img src="https://gitcode.com/PDFMathTranslate-next/PDFMathTranslate-next/star/badge.svg"></a> -->
  <!-- <a href="https://huggingface.co/spaces/reycn/PDFMathTranslate-Docker">
    <img src="https://img.shields.io/badge/%F0%9F%A4%97-Online%20Demo-FF9E0D"></a> -->
  <!-- <a href="https://www.modelscope.cn/studios/AI-ModelScope/PDFMathTranslate"> -->
    <!-- <img src="https://img.shields.io/badge/ModelScope-Demo-blue"></a> -->
  <!-- <a href="https://github.com/PDFMathTranslate-next/PDFMathTranslate-next/pulls">
    <img src="https://img.shields.io/badge/contributions-welcome-green"></a> -->
  <a href="https://t.me/+Z9_SgnxmsmA5NzBl">
    <img src="https://img.shields.io/badge/Telegram-2CA5E0?style=flat-squeare&logo=telegram&logoColor=white"></a>
  <!-- License -->
  <a href="./LICENSE">
    <img src="https://img.shields.io/github/license/PDFMathTranslate-next/PDFMathTranslate-next"></a>
  <a href="https://hosted.weblate.org/engage/pdfmathtranslate-next/">
    <img src="https://hosted.weblate.org/widget/pdfmathtranslate-next/svg-badge.svg" alt="translation status" /></a>
    <a href="https://deepwiki.com/PDFMathTranslate-next/PDFMathTranslate-next"><img src="https://deepwiki.com/badge.svg" alt="Ask DeepWiki"></a>
</p>

<a href="https://trendshift.io/repositories/12424" target="_blank"><img src="https://trendshift.io/api/badge/repositories/12424" alt="Byaidu%2FPDFMathTranslate | Trendshift" style="width: 250px; height: 55px;" width="250" height="55"/></a>

</div>

PDF scientific paper translation and bilingual comparison. Based on [BabelDOC](https://github.com/funstory-ai/BabelDOC). Additionally, this project is also the official reference implementation for calling BabelDOC to perform PDF translation.

> **This deployment branch uses a new web architecture.** The public service is a
> Rust API and practical React single-page application; FastAPI is an internal
> BabelDOC bridge. Google is built in. The first administrator registers at
> `/admin`, then stores DeepSeek or other HTTPS OpenAI-compatible providers in
> the encrypted server-side provider store. API keys are sent only when an
> administrator saves them, are not persisted by the browser, and are never
> returned by the backend. DeepSeek follows the current V4 Flash/Pro API. See
> [docs/architecture-rust-fastapi.md](./docs/architecture-rust-fastapi.md) for the
> lifecycle, cancellation guarantees, deployment, and verification commands.

## Portable Compose deployment

The supported server deployment needs only [`compose.yml`](./compose.yml).
GitHub Actions tests the Python, Rust, and React code and publishes the three
multi-architecture images to GHCR; the target server never compiles source.
This path supports rootful Docker on Linux, Docker Compose v2.24 or newer, and
a local POSIX filesystem. Open inbound TCP port `38100`, put the Compose file in
an empty directory, and run:

```bash
docker compose up -d --pull always --remove-orphans
```

首次启动通常需要几十秒；运行 `docker compose ps`，等待 `translator` 和
`web` 都显示 `healthy` 后再访问。

### Complete `compose.yml` (copy directly)

多数用户可以直接复制下面的完整内容，保存为 `compose.yml`，然后运行上面的
命令。无需填写服务器 IP、域名或证书，也不需要下载源码或创建 `.env` 文件。
启动后直接访问 `http://服务器IP:38100`。如需修改宿主机端口，可直接把下面
Compose 中的 `38100` 改成其他端口，或在启动命令前设置
`PDF2ZH_LISTEN=新端口`。

例如改为 `39000`：

```bash
PDF2ZH_LISTEN=39000 docker compose up -d --pull always --remove-orphans
```

<!-- BEGIN PORTABLE COMPOSE -->
```yaml
name: pdf2zh-next

# Fresh VPS:
#   docker compose up -d --pull always --remove-orphans
# The default public endpoint is HTTP on port 38100. No server IP, domain, TLS
# certificate, or .env file is required. Set PDF2ZH_LISTEN=39000 to choose a
# different public port, or PDF2ZH_LISTEN=127.0.0.1:38100 if a reverse proxy on
# the same host should be the only process reaching the app.
# Everything required for migration lives in ./data and ./secrets. Never run
# `docker compose down -v`, and always copy both directories with this file.

x-logging: &default-logging
  driver: json-file
  options:
    max-size: "10m"
    max-file: "3"

services:
  init:
    image: ghcr.io/fengyuchen1314/pdfmathtranslate-next-ops:${PDF2ZH_IMAGE_TAG:-sha-96358130149a1f041127c7053addd76a9cd0abbc}
    user: "0:0"
    restart: "no"
    network_mode: none
    cap_drop:
      - ALL
    cap_add:
      - CHOWN
      - DAC_OVERRIDE
      - FOWNER
    security_opt:
      - no-new-privileges:true
    logging: *default-logging
    volumes:
      - ./data/jobs:/data/jobs
      - ./data/state:/data/state
      - ./secrets/web:/run/pdf2zh-init-secrets/web
      - ./secrets/worker:/run/pdf2zh-init-secrets/worker

  translator:
    image: ghcr.io/fengyuchen1314/pdfmathtranslate-next-translator:${PDF2ZH_IMAGE_TAG:-sha-96358130149a1f041127c7053addd76a9cd0abbc}
    init: true
    restart: unless-stopped
    stop_grace_period: 60s
    mem_limit: 8g
    pids_limit: 512
    cap_drop:
      - ALL
    cap_add:
      # The root supervisor drops Uvicorn to 10001:10001. KILL is retained
      # only so it can destroy that different-UID translation process group.
      - KILL
      - SETGID
      - SETUID
    security_opt:
      - no-new-privileges:true
    logging: *default-logging
    expose:
      - "8000"
    depends_on:
      init:
        condition: service_completed_successfully
    environment:
      TZ: Asia/Shanghai
      PDF2ZH_DATA_ROOT: /data/jobs
      PDF2ZH_WORKER_TOKEN_FILE: /run/pdf2zh-secrets/token
      PDF2ZH_PUBLIC_WORKER_MODE: "1"
      PDF2ZH_MAX_ACTIVE_JOBS: "1"
      PDF2ZH_WORKER_JOB_RETENTION_SECONDS: "${PDF2ZH_WORKER_JOB_RETENTION_SECONDS:-21600}"
      PDF2ZH_WORKER_MAX_JOURNAL_EVENTS: "10000"
      PDF2ZH_CANCEL_HARD_TIMEOUT_SECONDS: "${PDF2ZH_CANCEL_HARD_TIMEOUT_SECONDS:-75}"
      PDF2ZH_SUPERVISOR_FAILURE_LIMIT: "${PDF2ZH_SUPERVISOR_FAILURE_LIMIT:-18}"
      PDF2ZH_SUPERVISOR_CHILD_STOP_SECONDS: "${PDF2ZH_SUPERVISOR_CHILD_STOP_SECONDS:-45}"
    volumes:
      - ./data/jobs:/data/jobs
      - ./secrets/worker:/run/pdf2zh-secrets:ro
    networks:
      - worker
    healthcheck:
      test:
        - CMD-SHELL
        - >-
          python -c "import json,urllib.request;
          token=open('/run/pdf2zh-secrets/token',encoding='utf-8').read().rstrip('\\r\\n');
          r=urllib.request.Request('http://127.0.0.1:8000/internal/health',headers={'x-worker-token':token});
          p=json.load(urllib.request.urlopen(r,timeout=3));
          assert p['status']=='ok' and p['provider']=='google' and p['max_active_jobs']==1 and p['epoch'] and p['container_boot_id']"
      interval: 10s
      timeout: 5s
      retries: 12
      start_period: 30s

  web:
    image: ghcr.io/fengyuchen1314/pdfmathtranslate-next-web:${PDF2ZH_IMAGE_TAG:-sha-96358130149a1f041127c7053addd76a9cd0abbc}
    init: true
    restart: unless-stopped
    stop_grace_period: 360s
    mem_limit: 1g
    pids_limit: 128
    cap_drop:
      - ALL
    security_opt:
      - no-new-privileges:true
    logging: *default-logging
    ports:
      - "${PDF2ZH_LISTEN:-38100}:7860"
    depends_on:
      init:
        condition: service_completed_successfully
      translator:
        condition: service_healthy
    environment:
      TZ: Asia/Shanghai
      PORT: "7860"
      PDF2ZH_DATA_ROOT: /data/jobs
      PDF2ZH_STATE_ROOT: /data/state
      PDF2ZH_FRONTEND_DIR: /app/frontend
      PDF2ZH_WORKER_URL: http://translator:8000
      PDF2ZH_WORKER_TOKEN_FILE: /run/pdf2zh-secrets/worker-token
      PDF2ZH_MASTER_KEY_FILE: /run/pdf2zh-secrets/provider-master-key
      PDF2ZH_SETUP_TOKEN_FILE: /run/pdf2zh-secrets/admin-setup-token
      PDF2ZH_CONCURRENCY: "1"
      PDF2ZH_MAX_ACTIVE_JOBS: "1"
      PDF2ZH_MAX_UPLOAD_BYTES: "104857600"
      PDF2ZH_UPLOAD_TIMEOUT_SECONDS: "${PDF2ZH_UPLOAD_TIMEOUT_SECONDS:-300}"
      PDF2ZH_LEASE_SECONDS: "${PDF2ZH_LEASE_SECONDS:-180}"
      PDF2ZH_RELEASE_GRACE_SECONDS: "${PDF2ZH_RELEASE_GRACE_SECONDS:-15}"
      PDF2ZH_CANCEL_RECONCILE_SECONDS: "${PDF2ZH_CANCEL_RECONCILE_SECONDS:-120}"
      PDF2ZH_WORKER_RECYCLE_SECONDS: "${PDF2ZH_WORKER_RECYCLE_SECONDS:-180}"
      PDF2ZH_TERMINAL_RETENTION_SECONDS: "21600"
      PDF2ZH_ID_TOMBSTONE_SECONDS: "${PDF2ZH_ID_TOMBSTONE_SECONDS:-21600}"
    volumes:
      - ./data/jobs:/data/jobs
      - ./data/state:/data/state
      - ./secrets/web:/run/pdf2zh-secrets:ro
    networks:
      - worker
    healthcheck:
      test: ["CMD", "curl", "--fail", "--silent", "http://127.0.0.1:7860/api/health"]
      interval: 15s
      timeout: 5s
      retries: 8
      start_period: 10s

networks:
  worker:
```
<!-- END PORTABLE COMPOSE -->

The first start creates `data/` and `secrets/` beside the Compose file. Read the
one-time administrator claim token locally on the server, then open
`http://服务器IP:38100/admin`:

```bash
sudo cat ./secrets/web/admin-setup-token
```

The first successful claim creates the administrator. That administrator adds
the DeepSeek key in the backend, or defines another HTTPS OpenAI-compatible
provider and its explicit model allow-list. Anonymous users can use only
Google. Provider API keys are not stored persistently by the browser and are
never returned to it by the backend.

The distributed Compose file already pins all three images to one tested Git
commit. To update, first back up the portable directory, replace it with the
next release's Compose file, and run the same command. `--remove-orphans`
removes services retired by the new Compose without deleting `data/` or
`secrets/`. To select another tested release explicitly, prefix the command
with `PDF2ZH_IMAGE_TAG=sha-<full-40-character-commit>`. No IP-specific setting
is stored. For migration, stop the service and copy the entire directory; the
Compose file, `data/`, and `secrets/` are the complete portable unit.

Raw HTTP does not protect the administrator password, session, or provider API
keys in transit. Use it only on a trusted network; for an Internet-facing admin
page, terminate HTTPS in your own Nginx, Caddy, Traefik, or control panel and
proxy to `http://127.0.0.1:38100`. The application automatically checks the
browser Origin against the forwarded `Host` and selects Secure cookies for an
HTTPS origin, so no domain needs to be added to Compose. The proxy must preserve
the original Host including a non-standard port, must not rewrite `Origin`, and
should disable request/response buffering with an upload limit of at least
102 MiB and a read timeout of at least one hour. For same-host proxying, change
the published port to `127.0.0.1:38100:7860` so port 38100 is not public.

Before packing a migration, wait until the translation queue is idle, stop the
stack, and archive from its parent directory so ownership and modes are
preserved. The archive contains API credentials, so encrypt it and restrict
access:

```bash
docker compose stop
sudo tar --acls --xattrs -czf ../pdf2zh-portable.tgz .
```

- 📊 Preserve formulas, charts, table of contents, and annotations _([preview](#preview))_.
- 🌐 Support [multiple languages](https://pdf2zh-next.com/supported_languages.html), and diverse [translation services](https://pdf2zh-next.com/advanced/Documentation-of-Translation-Services.html).
- 🤖 Provides [commandline tool](https://pdf2zh-next.com/getting-started/USAGE_commandline.html), [interactive user interface](https://pdf2zh-next.com/getting-started/USAGE_webui.html), and [Docker](https://pdf2zh-next.com/getting-started/INSTALLATION_docker.html)

<!-- Feel free to provide feedback in [GitHub Issues](https://github.com/PDFMathTranslate-next/PDFMathTranslate-next/issues) or [Telegram Group](https://t.me/+Z9_SgnxmsmA5NzBl). -->

> [!WARNING]
>
> This project is provided "as is" under the [AGPL v3](https://github.com/PDFMathTranslate-next/PDFMathTranslate-next/blob/main/LICENSE) license, and no guarantees are provided for the quality and performance of the program. **The entire risk of the program's quality and performance is borne by you.** If the program is found to be defective, you will be responsible for all necessary service, repair, or correction costs.
>
> Due to the maintainers' limited energy, we do not provide any form of usage assistance or problem-solving. Related issues will be closed directly! (Pull requests to improve project documentation are welcome; bugs or friendly issues that follow the issue template are not affected by this)


For details on how to contribute, please consult the [Contribution Guide](https://pdf2zh-next.com/community/Contribution-Guide.html).

<h2 id="preview">Preview</h2>

<div align="center">
<!-- <img src="./docs/images/preview.gif" width="80%"  alt="preview"/> -->
<img src="https://s.immersivetranslate.com/assets/r2-uploads/images/babeldoc-preview.png" width="80%"/>
</div>

<h2 id="demo">Online Service 🌟</h2>

You can try our application out using either of the following services:

- [Immersive Translate - BabelDOC](https://app.immersivetranslate.com/babel-doc/) Free usage quota is available; please refer to the FAQ section on the page for details.

<h2 id="install">Installation and Usage</h2>

### Installation

1. [**Windows EXE**](https://pdf2zh-next.com/getting-started/INSTALLATION_winexe.html) <small>Recommand for Windows</small>
2. [**Docker**](https://pdf2zh-next.com/getting-started/INSTALLATION_docker.html) <small>Recommand for Linux</small>
3. [**uv** (a Python package manager)](https://pdf2zh-next.com/getting-started/INSTALLATION_uv.html) <small>Recommand for macOS</small>

---

### Usage

1. [Using **WebUI**](https://pdf2zh-next.com/getting-started/USAGE_webui.html)
2. [Using **Zotero Plugin**](https://github.com/guaguastandup/zotero-pdf2zh) (Third party program)
3. [Using **Commandline**](https://pdf2zh-next.com/getting-started/USAGE_commandline.html)

For different use cases, we provide distinct methods to use our program. Check out [this page](./getting-started/getting-started.md) for more information.

<h2 id="usage">Advanced Options</h2>

For detailed explanations, please refer to our document about [Advanced Usage](https://pdf2zh-next.com/advanced/advanced.html) for a full list of each option.

<h2 id="downstream">Secondary Development (APIs)</h2>

<!-- <!-- For downstream applications, please refer to our document about [API Details](./docs/APIS.md) for futher information about: -->

- [Python API](./docs/en/advanced/API/python.md), how to use the program in other Python programs
<!-- - [HTTP API](./docs/APIS.md#api-http), how to communicate with a server with the program installed -->

<h2 id="langcode">Language Code</h2>

If you don't know what code to use to translate to the language you need, check out [this documentation](https://pdf2zh-next.com/advanced/Language-Codes.html)

<h2 id="acknowledgement">Acknowledgements</h2>

- [Immersive Translation](https://immersivetranslate.com) sponsors monthly Pro membership redemption codes for active contributors to this project, see details at: [CONTRIBUTOR_REWARD.md](https://github.com/funstory-ai/BabelDOC/blob/main/docs/CONTRIBUTOR_REWARD.md)

- [SiliconFlow](https://siliconflow.cn) provides a free translation service for this project, powered by large language models (LLMs).

- 1.x version: [Byaidu/PDFMathTranslate](https://github.com/Byaidu/PDFMathTranslate)


- backend: [BabelDOC](https://github.com/funstory-ai/BabelDOC)

- PDF Library: [PyMuPDF](https://github.com/pymupdf/PyMuPDF)

- PDF Parsing: [Pdfminer.six](https://github.com/pdfminer/pdfminer.six)

- PDF Preview: [Gradio PDF](https://github.com/freddyaboulton/gradio-pdf)

- Layout Parsing: [DocLayout-YOLO](https://github.com/opendatalab/DocLayout-YOLO)

- PDF Standards: [PDF Explained](https://zxyle.github.io/PDF-Explained/), [PDF Cheat Sheets](https://pdfa.org/resource/pdf-cheat-sheets/)

- Multilingual Font: see [BabelDOC-Assets](https://github.com/funstory-ai/BabelDOC-Assets)

- [Asynchronize](https://github.com/multimeric/Asynchronize/tree/master?tab=readme-ov-file)

- [Rich logging with multiprocessing](https://github.com/SebastianGrans/Rich-multiprocess-logging/tree/main)

- Documentation i18n using [Weblate](https://hosted.weblate.org/projects/pdfmathtranslate-next/) 


<h2 id="conduct">Before submit your code</h2>

We welcome the active participation of contributors to make pdf2zh better. Before you are ready to submit your code, please refer to our [Code of Conduct](https://pdf2zh-next.com/community/CODE_OF_CONDUCT.html) and [Contribution Guide](https://pdf2zh-next.com/community/Contribution-Guide.html).

<h2 id="contrib">Contributors</h2>

<!-- <a href="https://github.com/PDFMathTranslate-next/PDFMathTranslate-next/graphs/contributors">
  <img src="https://opencollective.com/PDFMathTranslate/contributors.svg?width=890&button=false" />
</a> -->

<!-- ![Alt](https://repobeats.axiom.co/api/embed/45529651750579e099960950f757449a410477ad.svg "Repobeats analytics image") -->

<h2 id="star_hist">Star History</h2>

<a href="https://star-history.com/#PDFMathTranslate-next/PDFMathTranslate-next&Date">
 <picture>
   <source media="(prefers-color-scheme: dark)" srcset="https://api.star-history.com/svg?repos=PDFMathTranslate-next/PDFMathTranslate-next&type=Date&theme=dark" />
   <source media="(prefers-color-scheme: light)" srcset="https://api.star-history.com/svg?repos=PDFMathTranslate-next/PDFMathTranslate-next&type=Date" />
   <img alt="Star History Chart" src="https://api.star-history.com/svg?repos=PDFMathTranslate-next/PDFMathTranslate-next&type=Date"/>
 </picture>
</a>
