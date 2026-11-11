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

### 7-1. 전제가 바뀌었다 — 공개 API 신설 (2026-11-10 확인)

그 뒤 네이버가 **MYBOX 공개 API를 열었습니다**(`developers.mybox.naver.com`,
응답 확인 2026-11-10). 인증은 OAuth가 아니라 MYBOX 웹에서 발급하는 **개인용 액세스
토큰**이고, 업로드·다운로드는 서명된 1회용 URL을 받아 그 URL로 주고받는 2단계
방식입니다. 그래서 **바이트 범위 읽기도, 부분 쓰기도 없고 파일 단위로만 오갑니다**
— 2차 백엔드는 이 전제 위에서 스테이징(내려받아 고치고 통째로 올리기)으로 가야
합니다. 엔드포인트 세부는 착수 시 원문으로 다시 확인합니다.

---

## 8. 쓰기는 1차에서 통과(write-through)

`write()`를 받으면 바로 백엔드에 씁니다. 구현이 단순하고 데이터 유실 창이 없는 대신,
작은 쓰기가 많으면 왕복 비용을 그대로 냅니다. 읽기는 이미 청크 캐시(기본 1 MiB)로
묶어 순차 읽기를 라인 속도에 가깝게 만들었습니다.

write-back(스테이징 파일 + close/fsync 시 업로드)은 2차 과제입니다. 캐시 무효화
경계(`write_epoch`)는 이미 들어가 있어 교체가 국소적입니다.

---

## 9. FTP 백엔드: 연결 풀 + 전송에 붙은 위치

FTP는 "파일시스템처럼 생긴 스트림 프로토콜"이라, 백엔드 트레이트에 맞추는 과정에서
세 가지를 정했습니다.

**연결 하나에 전송 하나.** 제어 연결은 동시에 하나의 데이터 전송만 운반합니다.
목록을 읽으면서 파일을 읽으려면 연결이 더 필요하므로 작은 풀(기본 4)을 두고 작업
단위로 빌려 씁니다. 서버가 계정당 동시 연결을 제한하는 경우가 흔해 상한을 설정값으로
뺐습니다.

**`pread`가 없다.** 오프셋 읽기는 `REST n` + `RETR`이고, 그러면 그 지점부터 파일
끝까지 흐르는 새 데이터 연결이 열립니다. VFS가 요청하는 1 MiB 청크마다 이걸 새로
하면 순차 읽기가 청크당 연결 하나씩을 먹습니다. 그래서 **열린 파일이 전송과 위치를
들고** 있다가, 이어지는 읽기는 열린 스트림에서 그대로 읽고 탐색일 때만 다시 엽니다.

**중간에 끊은 전송은 연결을 버린다.** `ABOR`는 서버마다 구현이 제각각이고, 동기화가
깨진 연결은 *다음* 명령에 이전 전송의 응답을 돌려줍니다. 끝까지 읽지 않고 끊은
전송은 그 연결을 풀에 반납하지 않고 닫습니다 — 재연결 비용이 조용한 오동작보다
쌉니다.

딸린 결론 세 가지도 코드에 그대로 적어 두었습니다. 배타적 생성(O_EXCL)과 원자적
교체가 FTP에 없어 `create`·`rename`은 먼저 확인하고 진행하며(그 사이의 경쟁은 FTP가
닫을 방법을 주지 않습니다), 자르기가 없어 0은 빈 `STOR`·축소는 64 MiB까지 재업로드로
처리하고, 여유 공간을 묻는 명령이 없어 `statfs`는 1 TiB 가상 볼륨을 보고합니다
(0을 보고하면 탐색기와 Finder가 복사를 아예 거부합니다).

테스트는 `libunftp`를 테스트 프로세스 안에서 띄워 실제 제어/데이터 연결로 검증합니다.
설치할 것이 없고, 목(mock)이 아니라 진짜 프로토콜을 지납니다.

### 9-1. 쓰기는 로컬에 모았다가 한 번에 올린다 (실측으로 바뀐 설계)

처음에는 읽기와 대칭으로 **`REST` + `STOR`로 오프셋 쓰기**를 구현했습니다. 테스트는
전부 통과했지만(테스트가 순차로 썼기 때문), m4pro에서 실제로 마운트해 2.4 MB 파일을
복사하자 **256 KiB까지만 맞고 그 뒤가 전부 0**인 파일이 만들어졌습니다. 디버그 로그가
이유를 바로 보여 줬습니다 — NFS 마운트 계층이 쓰기를 순서대로 내려보내지 않습니다:

```
write offset=0  131072  262144  393216   786432  655360  524288  917504
```

RFC 3659의 `REST`는 스트림 재시작용이고 `STOR`에 대해 서버가 무엇을 할지는 규정돼
있지 않습니다. 즉 **FTP에 임의 오프셋 쓰기는 없습니다**. 그래서 쓰기는 임시 파일에
받고(임의 접근은 로컬 파일시스템이 공짜로 해 줍니다), flush·close 때 **처음부터 끝까지
한 번의 `STOR`로** 올립니다. rclone과 curlftpfs도 같은 방식입니다.

대가는 write-back이라는 점입니다 — flush 전의 데이터는 이 컴퓨터에만 있습니다. 대신
얻은 것은 (a) 내용이 깨지지 않고, (b) 쓰는 동안 연결을 붙잡지 않아 같은 드라이브의
다른 작업이 풀 고갈로 멈추지 않는다는 것입니다(이것도 같은 실행에서 함께 드러난
증상입니다). 8절의 write-through 원칙은 SMB에 대해서는 그대로이고, FTP는 프로토콜이
그걸 허용하지 않아 예외입니다.

회귀 테스트는 그때 관찰된 순서(`0,1,2,6,5,4,3,7` 청크)를 그대로 재생합니다.

### 3-3. Finder 사이드바: 서버 행이 아니라 고정된 볼륨 행 (2026-11-11)

FUSE-T 드라이브에 Finder가 스스로 만들어 주는 "위치" 행은 **SMB 서버** 행이라,
누르면 파일이 아니라 공유 목록(마운트 이름의 폴더 하나)이 먼저 나옵니다. 실측:
`/Volumes` 아래 마운트, 서버 이름과 볼륨 이름을 같게·다르게, NFS 백엔드(사이드바에
안 뜸), `nobonjour`(`127.0.0.1` 행, 연결 중에서 멈춤), `local`(무시됨), FSKit
(macOS 26 전용) 모두 같았습니다. 진짜 NAS를 Finder로 직접 연결해도 동일합니다.

한 번에 파일이 열리는 행은 사용자가 볼륨을 사이드바로 끌어다 놓았을 때 생기는
**고정된 볼륨** 행입니다. 이것을 만드는 공식 API(`LSSharedFileList`)는 macOS 15에서
항목 0개를 돌려주고 삽입하면 크래시합니다. 그래서 `finder.rs`가 목록 파일
(`~/Library/Application Support/com.apple.sharedfilelist/*.sfl3`, NSKeyedArchiver
plist)을 직접 읽고 씁니다. 즐겨찾기(FavoriteItems)에 넣은 항목이 볼륨이면
`sharedfilelistd`가 위치(FavoriteVolumes)로 옮깁니다. Finder는 파일을 시작할 때만
읽어서(파일 변경도, `sharedfilelistd` 재시작도 반영되지 않음을 확인) 바꾼 뒤
`killall Finder`를 합니다 - Finder는 스스로 다시 뜹니다. 항목 이름은 무시되고
**폴더 이름**이 보이므로, 새 드라이브의 마운트 폴더는 드라이브의 표시 이름을 따릅니다.

마운트 성공 직후 고정하고(이미 있으면 아무 것도 안 함), 드라이브를 지우거나
폴더를 옮기면 뺍니다. 실패해도 드라이브는 그대로 쓰입니다.

고정된 항목은 경로가 아니라 **북마크**(볼륨 신원 + 경로)로 저장됩니다. `/Volumes/<이름>`에
마운트하면 북마크의 볼륨이 그 smbfs 볼륨 자체라서, 다시 마운트하면(신원이 바뀜)
북마크가 "파일이 없다"로 풀리지 않아 행이 죽습니다(실측: `/Volumes/QNAP`은 재마운트
뒤 실패, `~/MyDrives/qnap-2`는 여러 번 재마운트해도 정상, 꺼내기 아이콘까지 유지).
그래서 **마운트 폴더는 홈 아래(`~/MyDrives/<이름>`)** 입니다. Finder의 "컴퓨터" 보기에도
똑같이 원격 볼륨으로 올라오고, `/Volumes`가 요구하던 관리자 권한도 필요 없습니다.

같은 날 고친 것: 에이전트가 트레이 모드일 때 SIGTERM을 듣지 않아 `launchctl bootout`·
로그아웃 때 launchd가 강제 종료했고, FUSE-T 헬퍼가 고아로 남아 드라이브 이름과 포트를
쥐고 있었습니다(다음 마운트는 `<이름>-1`로 등록되고 옛 이름으로 접속해 실패·멈춤).
트레이 모드에서도 신호를 받아 정상 종료하고, 마운트 직전에 그 마운트 지점을 섬기는
주인 없는 헬퍼를 먼저 정리합니다.

**"위치"의 서버 행**: FUSE-T가 Bonjour로 광고하는 한 Finder가 자동으로 만드는 행이고,
서버 행에는 "사이드바에서 제거" 메뉴가 없습니다(실측: 새 탭에서 열기·정보 보기뿐).
개별로 숨길 수 없고, Finder 설정 → 사이드바 → **연결된 서버**를 끄는 것만이 방법입니다.
그러면 고정된 드라이브 행(즐겨찾기)만 남아 한 번에 열립니다. 사용자 Finder 전체 설정이라
앱이 바꾸지 않고, 사용자가 켜고 끕니다. 고정 항목은 Bonjour 이름이 같게 유지되면
`/Volumes` 아래에서도 재마운트를 견딥니다(실측). 깨졌던 것은 이름이 `QNAP-1`로 바뀐
탓이었고, 홈 아래 폴더는 경로로 풀리므로 이름과 무관하게 견딥니다.

**항목 하나만 갱신하는 API는 Apple 전용** (2026-11-11 확인). Finder는 공개
`LSSharedFileList*` API로 사이드바 항목을 넣고 빼며 `sharedfilelistd`가 변경을 즉시
알립니다. 그런데 `sharedfilelistd`는 `com.apple.private.coreservices.canaccessanysharedfilelist`
entitlement를 검사하고, 없는 프로세스에는 빈 목록을 주고 쓰기를 버립니다(C로 호출,
GUI 세션·LaunchAgent·osascript 모두 0개, 파일 불변, 로그 "missing entitlement").
서드파티는 이 entitlement를 받을 수 없으므로 파일 직접 쓰기 + Finder 재시작이 유일한
길입니다. 재시작은 항목이 실제로 바뀔 때(드라이브 추가 후 첫 연결·삭제·폴더 이동)만
일어나며, 사용자 결정으로 이대로 둡니다.

## 10. 폴더 목록 속도 (2026-11-11)

WAN 너머 QNAP(RTT 10ms, 요청당 서버 응답 ~30ms)에서 `ls -l`이 항목 14개에 2초,
Finder 폴더 열기가 수 초 걸렸습니다. fuser 요청 로그로 잰 원인과 조치:

| 원인 | 조치 | 효과 |
|---|---|---|
| `..` 조회가 `/Projects/..` 키로 캐시를 비켜 매번 서버에 감 | `RPath::join`이 `.`·`..`를 정규화 | 폴더 열 때마다 120ms 절약 |
| macOS가 항목마다 `._이름` 사이드카를 찾고, 없는 이름은 캐시가 없어 매번 서버에 감 | 없는 이름의 음성 캐시 + **방금 받은 목록에 없는 이름은 서버에 묻지 않고 "없음"**(대소문자 무시) | 항목당 25~70ms → 0 |
| stat 한 번이 create + basic + standard + close 4왕복 | `FileNetworkOpenInformation` 한 번으로 시간·크기·속성을 받아 3왕복 | stat 0.4~0.6s → 0.13s |
| FUSE-T는 요청을 한 스레드로 주고 우리가 inline으로 답해 왕복이 직렬 | lookup·getattr·readdir·read는 런타임 태스크로 넘겨 병렬 처리(쓰기는 그대로 순서 유지) | 항목 N개가 N왕복이 아니라 겹쳐서 진행 |
| 캐시 수명 2초라 Finder가 새로 고칠 때마다 다시 stat | 속성·목록 10초(자체 쓰기는 즉시 무효화) | 재방문 ~0.01s |

측정(콜드): Libraries(9) 1.19s→0.38s, Projects(14) 0.98s→0.41s, Products(38) 0.71s,
웜은 0.01~0.16s. 남는 비용은 폴더당 목록 1회 + 처음 보는 이름의 stat뿐입니다.
다른 기기에서 만든 파일은 최대 10초 뒤에 보입니다.

## 11. 클라우드 백엔드 공통층 + 네이버 MYBOX (2026-11-11)

MYBOX, Google Drive, OneDrive, Dropbox는 파일시스템 입장에서 같은 모양입니다:
모든 항목에 **id**가 있고, 폴더는 id로 나열하며, 파일은 서비스가 주는 URL로
통째로 내려받고 올립니다. 오프셋 쓰기는 어디에도 없습니다. 그래서 "파일시스템이
되는 일"(경로→id 해석, 열린 파일의 로컬 복사본, 닫을 때 업로드)은
`mydrives-backend-cloud`의 `CloudBackend`가 한 번 하고, 서비스별 코드는
`CloudApi`(list·mkdir·delete·rename·move·download·upload·quota) 열 개 남짓만
구현합니다. 다음 서비스는 이 트레이트 구현 하나씩입니다.

- 읽기: 범위 요청을 받는 서비스는 다운로드 URL에서 조각 단위로 바로 읽고, 받지
  않는 서비스(MYBOX)는 첫 읽기에 파일 전체를 로컬 복사본으로 받습니다.
- 쓰기: 항상 로컬 복사본에 쓰고 flush/close 때 한 번에 올립니다(FTP와 같은
  이유 — 커널은 쓰기를 순서 없이 조각으로 줍니다). `create` 뒤 아무것도 안 쓴
  파일(touch)도 close 때 빈 파일로 한 번 올라갑니다.
- 경로 해석: 드라이브 최상위에서 한 단계씩 목록을 받아 내려갑니다(폴더당 1회,
  10초 캐시). 이름 비교는 서비스의 대소문자 규칙을 따릅니다.
- rename: 같은 폴더면 rename, 다른 폴더면 move(+rename). 대상이 있으면 replace
  일 때만 먼저 지우고 진행(원자적이지 않음, `rename_replace=false`).
- rmdir는 비어 있을 때만(서비스는 통째로 지우므로 검사는 우리 몫).

**MYBOX Open API** (developers.mybox.naver.com, 2026-11-11 원문 확인):
베이스 `https://open-api.mybox.naver.com/v1`, `Authorization: Bearer mbx_pat_…`.
루트 목록 `GET /drive/resources`, 폴더 목록 `GET /drive/folders/{id}/resources`
(count≤1000, cursor 페이징), 속성 `GET /drive/resources/{id}`, 폴더 생성
`POST /drive/folders`, 삭제 `DELETE /drive/resources/{id}`(휴지통), 이동
`POST …/move {parentId}`, 이름 변경 `POST …/rename {name}`, 다운로드
`GET /drive/files/{id}/download` → 1회용 URL(10분) → GET, 업로드
`POST /drive/files {fileName,fileSize,isOverwrite,parentId}` → URL(48시간) →
`multipart/form-data`의 `Filedata`로 POST, 용량 `GET /drive/storage`.
한도: 다운로드 5만/일, 삭제 240/분, 검색 30/분. 루트는 id가 없어 `parentId`
생략으로 가리키며, 루트로의 move는 API가 받지 않아 거부합니다.

토큰은 OAuth가 아니라 **개인 액세스 토큰**(30·60·90·180일)이라 갱신이 없습니다.
만료되면 모든 호출이 401이고, 그 경우만 "토큰이 만료되었거나 잘못됨, 편집에서
새 토큰" 메시지로 바꿔 보냅니다. 토큰은 비밀번호와 같은 자리(키체인)에 둡니다.

검증: 백엔드 의미론은 메모리 모의 서비스로, MYBOX 요청 모양(경로·헤더·페이징·
2단계 업로드/다운로드·토큰 비노출)은 모의 HTTP 서버(wiremock)로 시험했습니다.
실제 계정에서의 확인(특히 업로드 URL의 응답, 다운로드 URL의 Range 지원 여부)은
사용자 토큰이 있어야 합니다. Range가 된다면 `range_reads=true`로 바꾸어 큰 파일을
통째로 받지 않게 할 수 있습니다.
