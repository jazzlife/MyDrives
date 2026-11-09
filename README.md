# MyDrives

원격 저장소(NAS, FTP, WebDAV, 클라우드 드라이브)를 **로컬 드라이브/폴더처럼** 쓰게 해 주는
Windows·macOS 백그라운드 에이전트. 트레이(Windows) / 메뉴바(macOS)에 상주하며,
마운트는 커널 파일시스템 계층을 통해 OS에 직접 노출됩니다.

Rust 코어 하나에 OS별 마운트 어댑터를 붙이는 구조입니다. 핵심 규칙은
**"OS에 가장 가까운 계층만 OS별로, 나머지는 전부 공용"** 입니다.

```
  트레이 / 메뉴바 ─┐
  CLI ──── IPC ─────┤
                    ▼
            MountManager ──── MountProvider   (WinFsp | FUSE | NFS | Dokan*)
                    │                  ▲
                    ▼                  │ 커널 콜백
                   Vfs  ── inode·핸들·메타데이터 캐시·읽기 청크
                    │
                    ▼
                 Backend  (SMB ✅ | FTP* | WebDAV* | Google Drive* | OneDrive* | Dropbox*)
                                                                    * 2차 구현 예정
```

## 1차 구현 범위 (현재 상태)

| 항목 | 상태 |
|---|---|
| 백엔드 트레이트 + 인메모리 참조 구현 | ✅ |
| **NAS (SMB2/3) 백엔드** — 순수 Rust, 3 OS 공용 | ✅ 실 서버 검증 |
| VFS (inode·핸들·메타데이터 캐시·읽기 청크·쓰기 epoch) | ✅ |
| **macOS 마운트** — 내장 NFS 클라이언트(커널 익스텐션 불필요) | ✅ 실기 검증 |
| **Windows 마운트** — WinFsp | ✅ x64 실마운트 검증 (Windows 11에서 드라이브 문자 `Z:`) |
| Linux 마운트 — FUSE (개발·CI용) | ✅ 구현 |
| 트레이 / 메뉴바 UI | ✅ 구현 (`--features tray`) |
| 에이전트 데몬 + CLI (로컬 소켓/네임드 파이프 IPC) | ✅ 검증 |
| 자격증명 — Keychain / Credential Manager | ✅ 최초 1회 입력, 이후 다시 묻지 않음 |
| 로그인 시 자동 시작 (LaunchAgent / `Run` 키) | ✅ 양 OS 검증 |
| 끊김 감지 + 자동 재연결 워치독 | ✅ |
| FTP·WebDAV·클라우드 드라이브 | ⏳ 2차 구현 |

설계 근거와 라이선스 판단: [docs/DECISIONS.md](docs/DECISIONS.md)
(검증 결과 전문·내부 구조·로드맵 문서는 아직 작성 전입니다.)

## 빌드

```bash
cargo build --release                     # 에이전트 + CLI (헤드리스)
cargo build --release --features tray     # 트레이/메뉴바 포함 (데스크탑 배포용)
cargo test --workspace --features tray    # 단위·통합 테스트 (macOS 65 / Windows 59)
```

요구사항

| OS | 마운트 계층 | 추가 설치 |
|---|---|---|
| macOS 13+ | 내장 NFS 클라이언트 | **없음** |
| macOS (선택) | macFUSE | macFUSE + `--features macfuse` |
| Windows 10/11 | WinFsp 2.x | [winfsp.dev/rel](https://winfsp.dev/rel/) + LLVM(빌드 시) |
| Linux | FUSE | `fusermount`(fuse2/3) 또는 root |

Windows 빌드는 **x64(`x86_64-pc-windows-msvc`)** 기준입니다. ARM64 Windows에서도
x64 에뮬레이션으로 그대로 돌아갑니다(WinFsp가 `winfsp-x64.dll`을 같이 설치합니다).
`winfsp-sys`가 bindgen을 쓰므로 빌드 머신에 LLVM이 필요합니다.

```powershell
winget install --id WinFsp.WinFsp
winget install --id LLVM.LLVM --architecture x64
$env:LIBCLANG_PATH = "C:\Program Files\LLVM\bin"
cargo build --release --features tray
```

## 빠른 사용법

```bash
# 1) 에이전트 실행 (사용자 세션에서! 아래 '왜 서비스가 아닌가' 참고)
mydrivesd run                  # 트레이 빌드면 메뉴바/트레이에 아이콘이 뜸
mydrivesd run --no-tray        # 헤드리스

# 2) NAS 등록 — 비밀번호는 stdin으로 받아 OS 키체인에 저장됩니다
echo 'mypassword' | mydrives add smb \
  --id nas --host 192.168.0.10 --share data --user admin \
  --drive Z: --mountpoint ~/MyDrives/nas --password-stdin --mount

# 3) 상태 확인 / 마운트 제어
mydrives status
mydrives unmount nas
mydrives mount nas
mydrives providers             # 이 빌드가 쓸 수 있는 마운트 계층

# 4) 이름 바꾸기 (보이는 이름만; 마운트 경로는 그대로)
mydrives rename nas "거북이 NAS"

# 5) 로그인 시 자동 시작 (트레이/메뉴바의 "로그인 시 자동 시작" 체크와 같은 동작)
mydrives autostart             # 현재 상태와 등록 위치
mydrives autostart off
mydrives autostart on

# 6) 업데이트
mydrives update                # 지금 아는 상태
mydrives update --check        # 바로 확인
mydrives update --now          # 받아 둔 버전을 지금 적용(에이전트 재시작)
```

설정 파일 위치는 `mydrives config-path`. 비밀번호는 설정 파일에 저장되지 않습니다.

비밀번호는 **한 번만** 입력합니다. macOS는 로그인 키체인에, Windows는 자격 증명
관리자에 저장되고 그 다음부터는 에이전트가 조용히 읽어 씁니다. macOS에서는
키체인 항목을 접근 권한 목록 없이(`security add-generic-password -A`) 저장하고
읽기도 `/usr/bin/security`로 합니다. 키체인 항목에 붙는 접근 권한은 **앱의 코드
서명으로 식별**되는데, 그 서명은 빌드할 때마다 바뀌어서 라이브러리로 읽으면
"…가 키체인의 항목에 접근하려고 합니다" 창이 영원히 다시 뜨기 때문입니다.
대가도 분명합니다 — 같은 사용자로 실행되는 다른 프로그램이 이 비밀번호를
물어보지 않고 읽을 수 있습니다([docs/DECISIONS.md](docs/DECISIONS.md)).

데몬 없이 한 번만 마운트해 보려면(개발·검증용):

```bash
mydrivesd dev-mount --kind smb --host 192.168.0.10 --share data \
  --user admin --password-env PW --mountpoint /tmp/nas
# Ctrl-C 로 언마운트
```

## 이름과 경로

드라이브에는 이름이 둘 있고, 가리키는 바가 다릅니다.

| | 기본값 | 쓰이는 곳 |
|---|---|---|
| **이름**(label) | **제품 이름** — NAS 이름, 서비스 이름 | 목록·메뉴에 보이는 이름 |
| **드라이브 이름**(id) | **공유 폴더 이름** | 마운트 경로(`~/MyDrives/<공유>`), 키체인 계정, 설정 키 |

NAS 이름은 주소에서 가져옵니다(`turtlelab.ddns.net` → `turtlelab`). SMB는 장비의
이름을 따로 알려 주지 않고, IP로 접속하면 가져올 이름 자체가 없습니다(그 경우 `NAS`).
그래서 **이름은 언제든 고칠 수 있습니다** — 창의 "이름" 칸 또는 `mydrives rename`.
경로와 id는 바뀌지 않으므로 이름을 바꿔도 마운트가 끊기지 않습니다.

## 자동 업데이트

태그를 밀면([`.github/workflows/release.yml`](.github/workflows/release.yml)) 각 타깃의
실행 파일과 `SHA256SUMS`가 릴리즈에 올라가고, 에이전트가 그것을 받아 적용합니다.

1. 기본 6시간마다 최신 릴리즈를 확인하고, 더 높은 버전이면 조용히 내려받아
   **체크섬을 검증**한 뒤 대기시킵니다(이 단계까지는 보이는 변화가 없습니다).
2. **열린 파일이 하나도 없을 때** 교체하고 에이전트를 다시 시작합니다. 작업 중이면
   미뤘다가 다음 시작 때 적용합니다. 트레이에서 "지금 적용"을 누를 수도 있습니다.
3. Windows는 실행 중인 `.exe`를 지울 수 없으므로 `*.old`로 밀어 두고 다음 시작 때 지웁니다.

설정은 `config.toml`의 `[update]` — `enabled`, `repo`, `check_interval_mins`.
**비공개 저장소**라면 기기마다 토큰이 한 번 필요합니다(공개 저장소는 불필요):

```bash
printf '%s' 'github_pat_...' | mydrives update --token-stdin   # OS 키체인에 저장
```

## 왜 서비스(데몬)가 아니라 사용자 세션 프로세스인가

Windows에서 **드라이브 문자는 로그온 세션 소유**입니다. LocalSystem 서비스가
세션 0에서 마운트한 `Z:`는 사용자의 탐색기에서 아예 보이지 않습니다. macOS도
LaunchDaemon은 메뉴바 UI를 띄울 수 없습니다. 그래서 MyDrives는 서비스가 아니라
**사용자 세션 에이전트**로 돌고, 로그인 시 자동 시작도 사용자 단위로 등록합니다.

| OS | 등록 위치 |
|---|---|
| macOS | `~/Library/LaunchAgents/app.MyDrives.agent.plist` (LaunchAgent, LaunchDaemon 아님) |
| Windows | `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` (서비스 아님) |
| Linux | `~/.config/autostart/mydrives.desktop` |

에이전트는 **시작할 때마다 이 등록을 자기 자신의 경로로 갱신**합니다. 빌드를 새로
받거나 설치 폴더를 옮겨도 로그인 항목이 사라진 바이너리를 가리키는 일이 없습니다.
설정은 `config.toml`의 `general.autostart`에 남고, 꺼 두면 다시 등록하지 않습니다.

프로덕션 배포에서는 macOS는 `SMAppService`(macOS 13+)로 등록하는 것이 정석입니다
([docs/DECISIONS.md](docs/DECISIONS.md) 참고).

## 알려진 제약 (1차)

- **쓰기는 통과(write-through)** 입니다. 원격에 바로 씁니다. 작은 쓰기가 많은
  워크로드(일부 DB·대용량 Office 저장)는 2차의 write-back 캐시에서 빨라집니다.
- **하드링크·심볼릭 링크 미지원.** macOS 클라이언트가 간헐적으로 `LINK`를
  요청하는데(일부 `cp` 경로) 거부됩니다. 실패 시 응용프로그램이 복사로 대체합니다.
- **확장 속성(xattr)·ACL 미전달.** `com.apple.quarantine` 같은 플래그는 보존되지 않습니다.
- macOS NFS 경로는 **클라이언트 로컬 락**(`locallocks`)을 씁니다. 같은 파일을
  여러 기기에서 동시에 잠그는 시나리오는 보호되지 않습니다(단일 사용자 드라이브 전제).
- 네이버 마이박스는 **공개 API가 없어** 포함하지 않았습니다(2026-07 레거시 API 종료,
  신규 API 미공개). 근거는 [docs/DECISIONS.md](docs/DECISIONS.md).

## 저장소 구조

```
crates/
  mydrives-core/           VFS, inode/핸들, 캐시, 설정, 자격증명, 마운트 매니저
  mydrives-backend-smb/    SMB2/3 백엔드 (순수 Rust)
  mydrives-mount-windows/  WinFsp 어댑터
  mydrives-mount-nfs/      루프백 NFSv3 서버 (macOS 기본, 커널 익스텐션 불필요)
  mydrives-mount-fuse/     FUSE 어댑터 (Linux, 선택적 macFUSE)
  mydrives-ipc/            제어 채널 (unix socket / named pipe, JSON 한 줄 프로토콜)
  mydrives-daemon/         mydrivesd — 상주 에이전트 + 트레이/메뉴바
  mydrives-cli/            mydrives — CLI
scripts/
  verify-posix.sh          실제 마운트 E2E (macOS/Linux)
```

자동 시작 등록은 별도 스크립트가 아니라 에이전트 안에
(`mydrives-core/src/autostart.rs`) 들어 있습니다 — 등록 위치가 에이전트 경로를
따라가야 하는데, 그 경로를 아는 것은 에이전트 자신이기 때문입니다.
