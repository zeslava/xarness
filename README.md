# xarness

## Установка

```bash
curl --proto '=https' --tlsv1.2 -sSf https://raw.githubusercontent.com/zeslava/xarness/main/install.sh | sh
```

Скрипт определяет ОС и архитектуру (Linux x86_64/aarch64, macOS arm64), скачивает последний релиз, проверяет контрольную сумму и ставит бинарник в `~/.local/bin`. Другой путь — через `INSTALL_DIR`:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://raw.githubusercontent.com/zeslava/xarness/main/install.sh \
  | INSTALL_DIR=/usr/local/bin sh
```
