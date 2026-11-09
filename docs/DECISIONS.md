# 설계 결정과 근거

1차 구현에서 내린 결정들과 그 근거입니다. 날짜가 붙은 사실은 2026-11-09에 확인했습니다.

---

## 1. Rust 코어 + OS별 마운트 어댑터 (네이티브 2개 작성 대신)

**결정:** 백엔드·VFS·캐시·에이전트는 Rust 공용 코드, OS 커널에 닿는 계층만 어댑터로 분리.

**이유:** 이 제품의 복잡도는 UI가 아니라 (a) 파일시스템 시맨틱, (b) 캐시/일관성,
(c) 백엔드 프로토콜에 있습니다. 세 가지 모두 OS와 무관합니다. 반대로 OS별 코드가
꼭 필요한 부분(커널 콜백, 드라이브 문자, 메뉴바)은 전체의 20% 미만입니다.
네이티브로 2개를 쓰면 (a)(b)(c)를 두 번 구현하고 두 번 디버깅해야 합니다.

실제로 이번 구현에서 발견한 버그 대부분(청크 경계 읽기, O_TRUNC 캐시, rename 후
핸들 재사용, 읽기 전용 속성 처리)은 **OS 중립 계층**에 있었습니다. 한 번 고치면
세 OS에서 고쳐집니다.

`MountProvider` 트레이트가 그 경계입니다. 새 마운트 계층을 추가할 때 VFS는 건드리지 않습니다.

---

## 2. Windows 마운트: WinFsp (1차)

| 후보 | 성능 | 라이선스 | Rust 바인딩 |
|---|---|---|---|
| **WinFsp 2.1** | Windows 캐시 매니저 통합, Dokan보다 ~2배 | GPLv3 또는 유료 상용 | `winfsp` 0.13.2+winfsp-2.1 (2026-10-20) 활발 |
| Dokany 2.3.1.1000 (2025-09-28) | 캐시 매니저 통합 없음 | LGPL + MIT (무료) | `dokan` 0.3.1+dokan206 (2022-12-08) — Dokan 2.0.6 대상, 현행과 어긋남 |
| Cloud Filter API (cldflt.sys) | OS 내장, placeholder/하이드레이션 | 무료 | 비공식 래퍼만 |

**결정: WinFsp.** 사용자가 **내부 사용 전용(상용 배포 없음)** 임을 확인했으므로
GPLv3 제약이 문제되지 않고, 유지되는 Rust 바인딩과 캐시 통합 이점을 그대로 받습니다.

**되돌릴 수 있게 둔 것:** `MountProvider` 트레이트. 상용 배포로 방향이 바뀌면
`dokan` 제공자를 추가하면 되고 VFS는 그대로입니다. 온디맨드(클라우드 placeholder)
UX가 필요해지면 Cloud Filter API 제공자를 같은 자리에 넣습니다.

출처: [winfsp.dev/com](https://winfsp.dev/com/) (상용 라이선스 조직 $6000/3년, 개인 $2400/3년),
[dokany 릴리스](https://github.com/dokan-dev/dokany/releases/tag/v2.3.1.1000),
[crates.io/winfsp](https://crates.io/crates/winfsp), [crates.io/dokan](https://crates.io/crates/dokan),
[Cloud Filter API 문서](https://learn.microsoft.com/en-us/windows/win32/cfapi/build-a-cloud-file-sync-engine).

### 2-1. WinFsp DLL은 **지연 로드**로 링크해야 한다

WinFsp 미설치 머신에서 `mydrivesd.exe`가 로드 시점에 `winfsp-x64.dll`을 찾다가
**STATUS_DLL_NOT_FOUND(0xC0000135)로 즉사**했습니다. `main`에 들어가지도 못해
사용자는 아무 메시지도 못 봅니다. 실제로 재현·수정했습니다.

- `winfsp` 크레이트의 `delayload` 기능은 기본 활성이지만, 링커 인자는
  **바이너리를 만드는 패키지의 build.rs**에서 넣어야 합니다. 라이브러리 크레이트에
  넣으면 cargo가 최종 exe 링크에 적용하지 않습니다 → `crates/mydrives-daemon/build.rs`.
- WinFsp 설치 프로그램은 `bin` 디렉터리를 PATH에 넣지 않으므로, 런타임에
  `%ProgramFiles(x86)%\WinFsp\bin\winfsp-x64.dll`을 절대 경로로 먼저 로드합니다
  (`MYDRIVES_WINFSP_DLL`로 재지정 가능).

검증: WinFsp 없는 Windows 11에서 `mydrivesd providers`가
`winfsp unavailable: ... Install it from https://winfsp.dev/rel/`로 정상 종료.

---

## 3. macOS 마운트: FUSE-T 우선, 내장 NFS 폴백 (둘 다 커널 익스텐션 없음)

| 후보 | 설치 부담 | 라이선스 | 비고 |
|---|---|---|---|
| **루프백 NFSv3 + 내장 클라이언트** | **없음** | 없음 | FUSE-T가 내부적으로 쓰는 것과 같은 원리, 의존성 0 |
| macFUSE 5.5.0 (Tahoe는 5.3.3+) | kext 승인·재부팅 | BSD 3-clause + 상용 조항 | 가장 성숙하나 Apple Silicon에서 보안 정책 변경 필요 |
| FUSE-T | 설치 필요(kext 아님) | 비상업용 무료, 상용 별도 계약 | 성숙도 낮음 |
| FSKit (macOS 15.4+) | 앱 익스텐션 | Apple 공식 | Swift 익스텐션 필요, macOS 26+에서 기능 확장 |
| File Provider | — | Apple 공식 | 가상 볼륨이 아님. 일반 파일시스템 구현 불가 |

**결정: 루프백 NFSv3.** 사용자가 "설치는 Rust만" 허용했고, 무엇보다 **스톡 Mac에서
아무 설치 없이 동작**하는 유일한 경로입니다. macFUSE는 `--features macfuse`로
선택 가능하게 열어 두었습니다(빌드 시 macFUSE 필요).

실측: macOS 15.7.7에서 `mount_nfs`가 **사용자 권한으로 성공**했습니다
(`used_sudo=false`, 마운트 소유자 = 로그인 사용자). 권한 상승이 필요한 환경에서는
`MYDRIVES_SUDO_MOUNT=1`로 `sudo -n`을 허용하거나, 배포 시 권한 헬퍼를 1회 설치합니다.

출처: [macFUSE 릴리스](https://github.com/macfuse/macfuse/releases),
[macFUSE 5.4.0 공지](https://macfuse.github.io/2026/09/07/macfuse-5.4.0.html),
[FUSE-T 라이선스](https://github.com/macos-fuse-t/fuse-t/blob/main/License.txt),
[FSKit 정리](https://github.com/indragiek/GHFS/blob/main/FSKIT.md),
[File Provider 한계](https://github.com/cryptomator/cryptomator/issues/1590),
[nfsserve](https://github.com/huggingface/nfsserve).

### 3-1. NFS 마운트 옵션은 `hard` + 로컬 락

측정으로 정한 값입니다.

- `soft,timeo=100`(10초)로 두었을 때 **77ms RTT 링크에서 300개 파일 생성 중 ~30%가
  조용히 실패**했습니다(쉘 루프는 오류를 무시하고 넘어갑니다). 네트워크 드라이브에서
  이는 데이터 유실입니다 → `hard,timeo=600,retrans=5`.
  서버는 우리 자신의 루프백 프로세스라 사라지지 않고, 백엔드가 끊기면 브리지가
  `NFS3ERR_JUKEBOX`(재시도하라)를 돌려주고 트레이가 "offline"을 표시합니다.
- `nolocks`로 두면 `fcntl` 락이 실패해 **SQLite가 "disk I/O error"** 를 냅니다
  (실제 재현). macOS는 `locallocks`, Linux는 `nolock`(= 클라이언트 로컬 락)으로
  전환해 해결했습니다. 단일 사용자 드라이브 전제이므로 로컬 락으로 충분합니다.

### 3-2. 기본값을 FUSE-T로 바꿈 (2026-11-10)

루프백 NFS 마운트는 **Finder 사이드바에 절대 올라오지 않습니다**. 측정으로 확인한
이유는 Finder의 "위치"가 DiskArbitration 볼륨과 NetFS로 붙은 서버만 나열하기
때문이고, `LSSharedFileList`는 macOS 26에서 빈 껍데기입니다. 사용자의 요구가
"외장하드처럼 드라이브로 뜨는 것"이라 기본 계층을 FUSE-T로 옮겼습니다. FUSE-T도
kext가 아니고, `probe()`가 미설치를 알려주면 그대로 NFS로 내려갑니다(설정에서
드라이브에 `provider`를 못 박으면 그 값이 우선).

사이드바에 보이려면 Finder 설정 → 사이드바 → **"연결된 서버"** 가 켜져 있어야
합니다. 이 Mac은 꺼져 있었고, 켜자 바로 나타났습니다. 행 이름은 볼륨이 아니라
**서버** 이름이라 `location` 옵션으로 드라이브 이름을 넣습니다.

FUSE-T에 맞추느라 고친 것들(모두 재현 후 수정):

| 증상 | 원인 | 조치 |
|---|---|---|
| 목록이 `bad file descriptor` | `opendir`가 핸들 0을 돌려줌 | 0이 아닌 핸들, 그리고 VFS 파일 핸들과 겹치지 않게 최상위 비트 사용 |
| 첫 복사에서 `fcopyfile failed` + **파일 깨짐** | 루트 디렉터리 핸들과 첫 파일 핸들이 같은 1 | 위와 동일 |
| `mv`·`ditto`·`rsync`가 "Permission denied" | 벤더 fuser가 macFUSE 4의 16바이트 `fuse_rename_in`을 읽어 이름이 8바이트 밀림 → ENOSYS | `macfuse-4-compat`을 `apple-abi`에 묶음 |
| 32KiB 쓰기에서 멈춤 | FUSE-T 채널은 소켓이라 요청이 쪼개져 도착 | 헤더 길이만큼 마저 읽음 |
| 언마운트 후 에이전트가 안 끝남 | 헬퍼가 소켓을 계속 열어 둠 | 언마운트 때 소켓 shutdown, EOF를 세션 종료로 처리 |
| sqlite 실패 | `setlk`/`getlk` 미구현 | 마운트 계층에 범위 잠금 테이블 (이 Mac의 모든 프로그램이 이 서버 하나를 거치므로 로컬 락이 곧 상호 배제) |
| 마운트 직후 경로가 빈 디렉터리 | 볼륨은 헬퍼가 1~2초 뒤에 붙임 | 마운트 테이블에 보일 때까지 기다린 뒤 성공 보고 |
| 재시작 후 `mount_smbfs` exit 66 | 묵은 마운트를 풀면 macOS가 `/Volumes/<이름>`을 **나중에** 지움 | 정리가 끝난 뒤 디렉터리 생성 |

이름 규칙: Bonjour 이름(`<location>._smb._tcp.local`)으로 들어가므로 ASCII만,
공백은 `-`(FUSE-T가 마운트 명령을 공백으로 쪼갭니다). 한글 이름이나 공백이 들어가면
마운트가 "Short read of FUSE request header"로 죽는 것을 확인했습니다. 앞에
`MyDrives-`를 붙이는 이유는 "거북이 NAS"에서 남는 것이 `NAS`뿐이라 같은 망의 진짜
NAS와 이름이 부딪히기 때문입니다.

처음 마운트할 때 macOS가 **"mydrivesd가 로컬 네트워크의 기기를 찾도록 허용"**
을 묻습니다. FUSE-T가 Bonjour로 붙기 때문이며, 거부하면 마운트가 실패합니다.

---

## 4. 에이전트는 사용자 세션 프로세스 (서비스 아님)

Windows의 드라이브 문자는 **로그온 세션 소유**입니다. 세션 0의 LocalSystem 서비스가
만든 `Z:`는 사용자 탐색기에 보이지 않습니다(WinFsp #416, Dokany #177/#240에서
반복 보고되는 바로 그 증상). macOS LaunchDaemon은 메뉴바 UI를 띠울 수 없습니다.

**결정:** 마운트는 사용자 세션 에이전트가 수행하고, 특권이 필요한 작업만 분리합니다.
자동 시작은 Windows `HKCU\...\Run`, macOS LaunchAgent(배포 시 `SMAppService`).

출처: [WinFsp #416](https://github.com/winfsp/winfsp/issues/416),
[SMAppService 정리](https://theevilbit.github.io/posts/smappservice/).

### 4-1. 자동 시작 등록은 에이전트가 스스로 한다

설치 스크립트가 아니라 에이전트 안(`mydrives-core/src/autostart.rs`)에 둡니다.
등록 내용이 **에이전트의 절대 경로**라서, 그 경로를 정확히 아는 주체가 에이전트
자신이기 때문입니다. 시작할 때마다 자기 경로로 다시 써 두므로 빌드를 옮기거나
업데이트해도 로그인 항목이 사라진 바이너리를 가리키지 않습니다.

구현에서 걸린 두 가지:

- **macOS에서 `launchctl bootstrap`을 호출하지 않습니다.** `enable()`은 항상 이미
  돌고 있는 에이전트 안에서 실행되는데, `RunAtLoad`가 붙은 plist를 지금 bootstrap
  하면 launchd가 **두 번째 에이전트**를 띄웁니다(같은 소켓·같은 마운트 지점 경쟁).
  `~/Library/LaunchAgents`에 plist를 두는 것 자체가 등록이고, 로그인할 때 launchd가
  읽습니다. 끄는 쪽도 `bootout`이 아니라 `launchctl disable` + plist 삭제입니다 —
  `bootout`은 요청한 당사자(에이전트)에게 SIGTERM을 보내 마운트를 끊어 버립니다.
- **plist에 `EnvironmentVariables`를 적습니다.** launchd는 거의 빈 환경으로 띄우기
  때문에, `/Volumes` 아래 마운트에 필요한 `MYDRIVES_SUDO_MOUNT` 같은 값이 없으면
  로그인 때 뜬 에이전트만 조용히 다르게 동작합니다.

Windows에서는 `Run` 키가 콘솔 앱을 띄우면 검은 콘솔 창이 바탕화면에 남습니다.
에이전트는 **자기 콘솔에 자기밖에 없을 때만**(`GetConsoleProcessList() == 1`)
`FreeConsole()`로 콘솔을 내려놓고, 로그를 `config_dir/agent.log`로 돌립니다.
터미널에서 직접 실행하면(셸이 같은 콘솔에 있으므로) 평소처럼 화면에 찍힙니다.

---

## 5. SMB 클라이언트: 순수 Rust `smb` 크레이트

`pavao`(libsmbclient)는 UNIX 전용이고 빌드 머신마다 Samba 개발 패키지가 필요합니다.
순수 Rust `smb` 0.12.1은 Windows·macOS·Linux에서 같은 코드로 빌드되고 SMB 2/3,
서명·암호화를 지원합니다. 실제 Samba 4.12 서버 대상 라이브 테스트 8개 통과.

SMB를 매핑할 때 걸린 지점(코드 주석에도 남김):

1. **EOF**: SMB는 파일 끝 읽기에 `STATUS_END_OF_FILE` 오류를 돌려줍니다. 그대로 올리면
   정상적인 `read()`가 I/O 오류가 되어 `cp`·Finder 복사가 마지막 블록에서 실패합니다
   → 빈 읽기로 변환.
2. **삭제**: 동사가 없습니다. `FILE_DELETE_ON_CLOSE`로 열고 닫는 방식은 비어 있지 않은
   디렉터리 삭제 실패가 **close 시점에 묻혀 "성공"으로 보고**됐습니다(실측).
   `SET_INFO(FileDispositionInformation)`로 바꿔 서버가 즉시
   `STATUS_DIRECTORY_NOT_EMPTY`를 돌려주게 했습니다. 삭제 실패를 성공으로 보고하는 것은
   최악입니다 — 탐색기는 항목을 지우고 사용자는 삭제됐다고 믿습니다.
3. **rename**: 열린 핸들에 `SET_INFO(FileRenameInformation)`, 새 이름은 **공유 루트 기준**.
4. **타임스탬프**: FILETIME(1601 기준 100ns)이고 `0`은 "변경하지 않음"입니다. 실제 0을
   넣으면 타임스탬프가 지워집니다.
5. **디렉터리 열거**: SMB2는 핸들당 동시 질의 1개뿐이라 `readdir`마다 핸들을 새로 엽니다.

---

## 6. 자격증명: OS 보안 저장소만

비밀번호/토큰은 설정 파일에 저장하지 않습니다. macOS Keychain, Windows Credential
Manager(`keyring` 3.x의 `apple-native`/`windows-native`). CLI는 `--password-stdin`으로만
받아 `ps`와 쉘 히스토리에 남기지 않습니다.

Linux는 배포 대상이 아니라서 D-Bus Secret Service를 끌어오지 않고 0600 파일로
대체하며, 사용할 때마다 경고를 남깁니다.

### 6-1. macOS는 키체인을 `/usr/bin/security`로 읽고 쓴다 (라이브러리 호출 아님)

macOS 키체인 항목에는 **접근 권한 목록(ACL)** 이 붙고, 거기 들어가는 애플리케이션
식별자는 **코드 서명**입니다. 에이전트를 다시 빌드하면 서명이 바뀌므로, 어제 저장한
비밀번호를 오늘 빌드가 읽으면 `"…가 키체인의 항목에 접근하려고 합니다"` 창이 뜹니다.
매 빌드·매 업데이트마다 다시 뜨고, 로그인 직후처럼 창을 띄울 수 없는 상황에서는
읽기가 그대로 **무한 대기**합니다(실제로 에이전트가 소켓도 못 열고 멈췄습니다).

m4pro(macOS 26)에서 측정한 결과:

| 저장 방법 | 읽기 방법 | 결과 |
|---|---|---|
| `keyring`(Security 프레임워크) | 같은 라이브러리, 새 빌드 | 인증 대화상자 → 차단 |
| `security -A` (ACL 없음) | 같은 라이브러리 | **여전히 대화상자** |
| `security -A` (ACL 없음) | `security find-generic-password -w` | 0.1초, 질문 없음 |

그래서 저장·조회·삭제 모두 `/usr/bin/security`를 거칩니다. 읽는 주체가 애플이 서명한
고정된 바이너리 하나로 고정되고, 우리 바이너리의 정체성은 키체인 판단에서 빠집니다.
저장은 `-U`(갱신)가 아니라 **삭제 후 새로 추가**합니다. 갱신은 기존 ACL을 그대로
물려받아서, 고치려는 문제가 그대로 남기 때문입니다.

대가는 분명히 적어 둡니다: `-A`는 "이 사용자로 도는 모든 프로그램이 질문 없이 읽을 수
있음"입니다. 같은 사용자 권한이면 이미 설정 파일·마운트·사용자 파일에 전부 접근할 수
있으므로 받아들였습니다. 코드 서명이 고정된 정식 배포판(공증된 `.app`)에서는 서명이
빌드마다 바뀌지 않으므로 ACL을 유지하는 쪽으로 되돌릴 수 있습니다.

Windows 자격 증명 관리자에는 이 문제가 없습니다(사용자 단위 저장, 프롬프트 없음).
`mydrives add` → `cmdkey /list`에 `LegacyGeneric:target=<id>.app.MyDrives`로 저장되고
`mydrives rm`에서 함께 지워지는 것을 vm-win에서 확인했습니다.

---

## 7. 네이버 마이박스: 1차에서 제외

네이버가 레거시 API를 종료해 **2026-07-14 기준 공개 API가 없습니다**. RaiDrive 같은
외부 개발사도 연동이 중단된 상태이고 신규 API는 미공개입니다. 비공식 리버스
엔지니어링은 약관 위험이 있어 구현하지 않고, 상태를 명시만 했습니다.

출처: [RaiDrive 공지](https://ko-with.raidrive.com/t/topic/7388).

---

## 8. 쓰기는 1차에서 통과(write-through)

`write()`를 받으면 바로 백엔드에 씁니다. 구현이 단순하고 데이터 유실 창이 없는 대신,
작은 쓰기가 많으면 왕복 비용을 그대로 냅니다. 읽기는 이미 청크 캐시(기본 1 MiB)로
묶어 순차 읽기를 라인 속도에 가깝게 만들었습니다.

write-back(스테이징 파일 + close/fsync 시 업로드)은 2차 과제입니다. 캐시 무효화
경계(`write_epoch`)는 이미 들어가 있어 교체가 국소적입니다.
