# Wuwa Tappo for macOS (unofficial)

명조: 워더링 웨이브의 데스크톱 펫 **Wuwa Tappo**(Windows 전용)를 macOS에서 실행하기 위한 비공식 호스트입니다.

이 저장소에는 쿠로게임즈의 화면 코드나 그림 자원이 **들어 있지 않습니다**. 공식 Windows 설치 파일을 직접 받아 `tools/build.sh`를 실행하면, 설치 파일에서 화면을 추출해 macOS 앱(`Wuwa Tappo.app`)으로 빌드합니다. 이 저장소에 들어 있는 것은 그 화면이 호출하는 기능(창, 전역 입력, 트레이, 저장 등)을 macOS용으로 다시 구현한 Tauri 호스트와, 추출·패치·검사 도구입니다.

> Unofficial macOS host for Kuro Games' Windows desktop pet *Wuwa Tappo*. No Kuro code or art is included; `tools/build.sh` extracts the frontend from your own copy of the official installer and builds a macOS app around it. Not affiliated with or endorsed by Kuro Games.

## 준비물

- Apple Silicon 또는 Intel Mac, macOS 11 이상
- 공식 설치 파일 `WuwaTappo-global_<버전>_x64-setup.exe`. 공식 다운로드 페이지에서 받습니다.
- Xcode Command Line Tools: `xcode-select --install`
- Homebrew 패키지: `brew install sevenzip brotli rustup`
- Rust와 Tauri CLI:
  ```sh
  rustup default stable
  export PATH="$(brew --prefix rustup)/bin:$HOME/.cargo/bin:$PATH"
  cargo install tauri-cli --version '^2' --locked
  ```

## 빌드와 설치

```sh
git clone https://github.com/atimevil/wuwa-tappo-mac.git
cd wuwa-tappo-mac
tools/make_signing_identity.sh        # 처음 한 번만, 선택 사항 (아래 "권한" 참고)
tools/build.sh ~/Downloads/WuwaTappo-global_1.0.0_x64-setup.exe
```

`build.sh`는 다음을 순서대로 처리합니다.

1. 설치 파일에서 화면을 추출합니다.
2. macOS용 패치를 적용합니다.
3. 호스트와 화면의 호출 규약이 맞는지 검사합니다.
4. 아이콘을 만들고 앱을 빌드·서명한 뒤 `/Applications`에 설치합니다.

설치하지 않고 빌드만 하려면 끝에 `--no-install`을 붙입니다. 결과물은 `src-tauri/target/release/bundle/macos/`에 생깁니다.

## 권한

키 입력과 클릭 수를 세려면 **시스템 설정 → 개인정보 보호 및 보안 → 입력 모니터링**에서 Wuwa Tappo를 허용해야 합니다. 허용하면 macOS가 앱을 한 번 종료하므로 다시 열면 됩니다.

macOS의 요청 창은 앱마다 한 번만 뜨고, 그 창을 놓치면 설정 목록에 앱이 나타나지 않을 수도 있습니다. 그래서 권한이 없으면 앱이 시작할 때마다 직접 안내 창을 띄웁니다. 안내 창에서 "설정 열기"를 누른 뒤 목록에서 Wuwa Tappo를 켜면 됩니다. 목록에 없으면 아래 "+"를 눌러 응용 프로그램 폴더의 Wuwa Tappo를 추가하세요.

macOS는 이 권한을 앱의 서명에 묶어 둡니다. 따로 서명하지 않으면 다시 빌드할 때마다 서명이 바뀌어 권한을 다시 줘야 합니다. `tools/make_signing_identity.sh`는 로그인 키체인에 자체 서명 인증서(`WuwaTappo Local Signing`)를 하나 만듭니다. 이후 `build.sh`가 이 인증서로 서명하므로, 다시 빌드해도 권한이 유지됩니다. 처음 서명할 때 키체인 암호를 묻는데, "항상 허용"을 고르면 됩니다. 인증서는 키체인 접근 앱에서 언제든 지울 수 있습니다.

## 로그인

- **쿠로 계정:** Safari에서 공식 로그인 페이지가 열립니다. 로그인이 끝나면 `wuwa-buddy://` 링크로 앱에 돌아옵니다. Firefox에서는 이 페이지의 로그인 SDK가 초기화되지 않아(오류 20003), 기본 브라우저와 관계없이 Safari로 엽니다.
- **게임 UID:** 앱 안에서 인증 코드로 로그인합니다.

## 동작 방식

Windows 앱은 Tauri 2 앱입니다. 화면(Vue)은 실행 파일 안에 압축되어 있고, Rust 쪽이 창·전역 입력·트레이·저장을 맡습니다. 이 저장소는 그 Rust 쪽을 macOS용으로 다시 구현했습니다.

| 부분 | 파일 |
|---|---|
| 화면이 부르는 명령 51개, 창, 메뉴, 저장 | `src-tauri/src/lib.rs` |
| 전역 키보드·마우스 (CGEventTap, 리스닝 전용) | `src-tauri/src/input.rs` |
| 실시간 서버 웹소켓 중계 | `src-tauri/src/ws_proxy.rs` |
| 설치 파일 해제, 자원 추출 | `tools/extract_payload.py`, `tools/extract_assets.py` |
| 화면 패치 (Windows와 macOS의 차이 보정) | `tools/patch_frontend.py` |

### Windows와 다른 점

- **쿠키:** 화면 주소가 `tauri://localhost`라 WebKit이 쿠키를 저장하지 않습니다. 로그인 토큰이 쿠키에 담기므로, 모든 창에서 `document.cookie`를 localStorage 기반 저장소로 대체합니다.
- **실시간 서버(socket.io):** 서버는 웹소켓 연결을 `http(s)://tauri.localhost` 출처나 Origin 헤더가 없을 때만 받아 줍니다. 그래서 그 서버로 가는 웹소켓은 앱 안의 루프백 중계기(127.0.0.1)를 거칩니다. 중계기는 연결 요청의 Host와 Origin만 바꾸고 TLS 연결은 그대로 이어 줍니다.
- **창 열기 이벤트:** 로그인 창처럼 이벤트를 받아야 화면이 그려지는 창이 있습니다. 이런 창에는 페이지가 이벤트 수신을 등록한 뒤에 이벤트를 보냅니다(패치 1).
- **클릭 통과:** 화면 전체를 덮는 투명 창이, 화면이 알려 준 펫 영역 위에서만 클릭을 받습니다.
- **기타:**
  - 자동 업데이트는 꺼져 있습니다. 업데이트 파일이 Windows 설치 파일이기 때문입니다.
  - 패널을 접었을 때의 폭(112px)은 CSS를 보고 정한 추정값입니다.
  - 메뉴 막대 아이콘이 많으면 트레이 아이콘이 MacBook 노치 뒤에 가려질 수 있습니다. 같은 메뉴를 펫 우클릭으로도 열 수 있습니다.

## 새 버전이 나왔을 때

새 설치 파일로 `tools/build.sh`를 다시 실행하면 됩니다. 패치나 호출 규약 검사가 실패하면 화면 코드가 바뀐 것입니다. 이때는 `tools/patch_frontend.py`의 원문 문자열과 `src-tauri/src/lib.rs`의 명령 목록을 새 버전에 맞게 고쳐야 합니다.

## 검사 도구

- `python3 tools/check_contract.py`: 화면이 보내는 모든 명령·인자 이름을 Rust 쪽 구현과 대조합니다. 플러그인 명령이 권한 목록에 있는지도 확인합니다.
- `python3 tools/check_cookie_shim.py`: 쿠키 대체 코드를 화면 자체의 쿠키 함수로 검사합니다. Node가 필요합니다.
- 런타임 명령 검사(디버그 빌드 전용): `tools/build.sh`로 `web/`을 한 번 만든 뒤 아래를 실행합니다.
  ```sh
  (cd src-tauri && cargo build)
  WUWA_QA_SCRIPT=tools/qa_invoke.js src-tauri/target/debug/wuwatappo-mac
  ```
  앱 로그(`~/Library/Logs/io.github.atimevil.wuwatappo-mac/`)에서 `[QA]` 줄을 보면 되고, `done pass=… fail=0`으로 끝나야 정상입니다.
- `cd src-tauri && cargo test`

## 라이선스와 고지

이 저장소의 코드는 MIT 라이선스입니다(`LICENSE`). Wuwa Tappo, 명조: 워더링 웨이브와 관련 이름·그림·화면 코드의 권리는 Kuro Games에 있습니다. 이 프로젝트는 Kuro Games와 관계가 없고, 승인받지 않았습니다. 빌드한 앱이나 추출한 자원을 다른 사람에게 배포하지 마세요.
