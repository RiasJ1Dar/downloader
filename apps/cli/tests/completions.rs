use std::process::Command;

#[test]
fn completions_pwsh_генерує_скрипт_для_powershell() {
    let output = Command::new(env!("CARGO_BIN_EXE_dl"))
        .args(["completions", "pwsh"])
        .output()
        .expect("виклик dl completions pwsh");

    assert!(output.status.success(), "dl completions pwsh має завершуватись успішно");
    let stdout = String::from_utf8(output.stdout).expect("валідний UTF-8");
    assert!(
        stdout.contains("Register-ArgumentCompleter"),
        "PowerShell-скрипт має містити Register-ArgumentCompleter"
    );
    assert!(
        stdout.contains("'dl'"),
        "PowerShell-скрипт має реєструвати команду 'dl'"
    );
}

#[test]
fn completions_powershell_псевдонім_працює() {
    let output = Command::new(env!("CARGO_BIN_EXE_dl"))
        .args(["completions", "powershell"])
        .output()
        .expect("виклик dl completions powershell");

    assert!(output.status.success(), "dl completions powershell має завершуватись успішно");
    let stdout = String::from_utf8(output.stdout).expect("валідний UTF-8");
    assert!(
        stdout.contains("Register-ArgumentCompleter"),
        "PowerShell-скрипт має містити Register-ArgumentCompleter"
    );
}

#[test]
fn completions_bash_генерує_скрипт() {
    let output = Command::new(env!("CARGO_BIN_EXE_dl"))
        .args(["completions", "bash"])
        .output()
        .expect("виклик dl completions bash");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("валідний UTF-8");
    assert!(
        stdout.contains("_dl") || stdout.contains("complete -F"),
        "Bash-скрипт має містити функцію автодоповнення"
    );
}

#[test]
fn completions_zsh_генерує_скрипт() {
    let output = Command::new(env!("CARGO_BIN_EXE_dl"))
        .args(["completions", "zsh"])
        .output()
        .expect("виклик dl completions zsh");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("валідний UTF-8");
    assert!(
        stdout.contains("compdef _dl dl") || stdout.contains("_dl"),
        "Zsh-скрипт має містити compdef"
    );
}

#[test]
fn completions_fish_та_elvish_генерують_скрипти() {
    let fish = Command::new(env!("CARGO_BIN_EXE_dl"))
        .args(["completions", "fish"])
        .output()
        .expect("fish");
    assert!(fish.status.success());
    let fish_out = String::from_utf8(fish.stdout).expect("fish UTF-8");
    assert!(fish_out.contains("complete -c dl"));

    let elvish = Command::new(env!("CARGO_BIN_EXE_dl"))
        .args(["completions", "elvish"])
        .output()
        .expect("elvish");
    assert!(elvish.status.success());
    let elvish_out = String::from_utf8(elvish.stdout).expect("elvish UTF-8");
    assert!(elvish_out.contains("edit:completion:arg-completer[dl]"));
}

#[test]
fn згенерований_файл_підхоплюється_в_powershell() {
    let output = Command::new(env!("CARGO_BIN_EXE_dl"))
        .args(["completions", "pwsh"])
        .output()
        .expect("dl completions pwsh");

    assert!(output.status.success());

    // Для сумісності з Windows PowerShell 5.1 файли .ps1 на диску
    // з кирилицею потребують UTF-8 BOM, інакше інтерпретуються як ANSI (CP1251).
    let mut file_content = vec![0xEF, 0xBB, 0xBF];
    file_content.extend_from_slice(&output.stdout);

    let temp_dir = std::env::temp_dir();
    let script_path = temp_dir.join(format!("dl_completion_test_{}.ps1", std::process::id()));
    std::fs::write(&script_path, &file_content).expect("запис тестового ps1");

    let ps = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
            script_path.to_str().expect("UTF-8 шлях"),
        ])
        .output();

    let _ = std::fs::remove_file(&script_path);

    if let Ok(res) = ps {
        let stderr = String::from_utf8_lossy(&res.stderr);
        assert!(
            res.status.success(),
            "PowerShell має виконати згенерований файл автодоповнення без помилок. Stderr: {stderr}"
        );
        assert!(
            stderr.trim().is_empty(),
            "PowerShell не повинен виводити помилок при завантаженні completer: {stderr}"
        );
    }
}

#[test]
fn згенерований_скрипт_підхоплюється_через_invoke_expression_в_powershell() {
    let dl_exe = env!("CARGO_BIN_EXE_dl");
    let cmd_str = format!("& '{dl_exe}' completions pwsh | Out-String | Invoke-Expression");

    let ps = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &cmd_str])
        .output();

    if let Ok(res) = ps {
        let stderr = String::from_utf8_lossy(&res.stderr);
        assert!(
            res.status.success(),
            "PowerShell має підхопити скрипт автодоповнення через Invoke-Expression: {stderr}"
        );
        assert!(
            stderr.trim().is_empty(),
            "Stderr має бути порожнім: {stderr}"
        );
    }
}

#[test]
fn згенерований_файл_підхоплюється_в_pwsh() {
    let output = Command::new(env!("CARGO_BIN_EXE_dl"))
        .args(["completions", "pwsh"])
        .output()
        .expect("dl completions pwsh");

    assert!(output.status.success());

    let temp_dir = std::env::temp_dir();
    let script_path = temp_dir.join(format!("dl_pwsh_test_{}.ps1", std::process::id()));
    std::fs::write(&script_path, &output.stdout).expect("запис тестового ps1");

    let pwsh = Command::new("pwsh")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
            script_path.to_str().expect("UTF-8 шлях"),
        ])
        .output();

    let _ = std::fs::remove_file(&script_path);

    if let Ok(res) = pwsh {
        let stderr = String::from_utf8_lossy(&res.stderr);
        assert!(
            res.status.success(),
            "pwsh має виконати згенерований файл без помилок: {stderr}"
        );
        assert!(stderr.trim().is_empty());
    }
}
