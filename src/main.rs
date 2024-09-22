use derive_more::{Display, From, Into};
use is_executable::IsExecutable;
use log::{debug, error, info, log_enabled, trace, warn, Level};
use env_logger::Env;
use regex::Regex;
use serde::Deserialize;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::env;
use std::ffi::OsString;
use std::fmt::{self, format};
use std::fs;
use std::fs::DirEntry;
use std::fs::File;
use std::io;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;
use symlink::symlink_file;
use tempfile::Builder;
#[derive(Debug, Deserialize)]

struct Plugin {
    name: String,
    path: PathBuf,
    alias: Option<String>,
    desc: Option<String>,
    fns: HashMap<String, Fun>,
    sources: Vec<PathBuf>,
    #[serde(default)]
    fn_template: Option<String>,
    #[serde(default)]
    fn_table_template: Option<String>,
    binds: Keybinds, //todo: convert to Vec
}

#[derive(Debug, Clone)]
struct InitialPlugin {
    name: String,
    path: PathBuf,
    alias: Option<String>, // Not implemented
    desc: Option<String>,  // Not implemented
}

// todo: custom one-pass replacement?
impl Plugin {
    fn fn_table(&self, global_config: &GlobalConfig) -> Option<String> {
        let mut sorted:  Vec<&Fun> = self
            .fns
            .values()
            .filter(|&fun| !fun.flags.contains(&FnFlag::NA))
            .collect();
            sorted.sort_by(|a, b| a.name.cmp(&b.name));
            
        let table_rows = sorted
                .into_iter()
                .map(|fun| {
                templatize(
                    fun.into(),
                    self,
                    &self
                        .fn_table_template
                        .as_deref()
                        .unwrap_or(&global_config.fn_table_template),
                    global_config,
                    false,
                    false
                )
            })
            .collect::<Vec<String>>();
        if table_rows.is_empty() {
            None
        } else {
            Some(table_rows.join("\n"))
        }
    }

    fn extra_table(&self, global_config: &GlobalConfig) -> Option<String> {
        let mut lines: Vec<String> = Vec::new();
        for fun in self.fns.values() {
            let provisioned_cmd=fun.get_cmd(self, global_config);
            let cmd= fun.get_real_cmd(self, global_config);
            let prefix = if provisioned_cmd == cmd {"command "} else {""};

            if !fun.flags.contains(&FnFlag::AL) && !fun.flags.contains(&FnFlag::PG) {
                if let Some(ref alias) = fun.alias {
                    if !alias.is_empty()  {
                        lines.push(format!(
                            "alias {}=\"{}\"",
                            alias,
                            &cmd
                        ));
                    } else {
                        warn!("alias for {:?} is empty, skipping.", fun)
                    }
                }
            }
            if fun.flags.contains(&FnFlag::WJR) {
                if fun.flags.contains(&FnFlag::WJR) {
                    lines.push(format!(
                        "{}() {{ if zle; then local INIT_BUFFER=$BUFFER; local INIT_CURSOR=$CURSOR; {}{}; BUFFER=$INIT_BUFFER; CURSOR=$INIT_CURSOR; zle redisplay; else {}{} $@; fi; }}",
                        &provisioned_cmd,
                        &prefix,
                        &cmd,
                        &prefix,
                        &cmd
                    ));
                } else { // not sure if we want this
                    lines.push(format!(
                        "{}() {{ if zle; then zle push-input; BUFFER=\"{}{}\"; zle accept-line; else {}{} $@; fi; }}",
                        &provisioned_cmd,
                        &prefix,
                        &cmd,
                        &prefix,
                        &cmd
                    ));
                }
                lines.push(format!("zle -N {}", &provisioned_cmd));
            } else if fun.flags.contains(&FnFlag::PBG) {
                lines.push(format!(
                    "{}() {{ pueue add  --escape -- {}{} $@ >/dev/null 2>&1; }}",
                    &provisioned_cmd,
                    &prefix,
                    &cmd
                ));
            } else if fun.flags.contains(&FnFlag::WJSUB) {
                lines.push(format!(
                    "{}() {{ if zle; then LBUFFER+=\"$({}{} | tr '\n' ' \\\n') \"; else {}{} $@; fi }}",
                    &provisioned_cmd,
                    &prefix,
                    &cmd,
                    &prefix,
                    &cmd
                ));
                lines.push(format!("zle -N {}", &provisioned_cmd));
            } else if fun.flags.contains(&FnFlag::WR) { // temporarily run a command
                lines.push(format!(
                    "{}() {{ if zle; then zle push-input; BUFFER=\"{}{} \"; else {}{} $@; fi; }}",
                    &provisioned_cmd,
                    &prefix,
                    &cmd,
                    &prefix,
                    &cmd
                ));
                lines.push(format!("zle -N {}", &provisioned_cmd));
            } else if fun.flags.contains(&FnFlag::WSUB) {
                lines.push(format!(
                    "{}() {{ if zle; then local wgArgs; vared -p \"Args: \" -c wgArgs; LBUFFER+=\"$({}{} ${{(z)wgArgs}}  | tr '\n' ' \\\n') \"; else {}{} $@; fi }}",
                    &provisioned_cmd,
                    &prefix,
                    &cmd,
                    &prefix,
                    &cmd
                ));
                lines.push(format!("zle -N {}", &provisioned_cmd));
            }

            // Generate the lines for each bind
            lines.extend(
                fun.binds
                    .iter()
                    .filter(|kb| !kb.is_empty())
                    .map(|kb| format!("bindkey '{}' \"{}\"", kb, &provisioned_cmd)),
            );
        }
        lines.extend(self.binds.iter().map(|kb| {
            format!(
                "bindkey '{}' \"{}\"",
                kb,
                templatize_simple(self, &global_config.selector_widget_template)
            )
        }));
        if lines.is_empty() {
            None
        } else {
            lines.push("\n".to_string());
            Some(lines.join("\n"))
        }
    }

    fn generated_filepath(&self, global_config: &GlobalConfig) -> PathBuf {
        return self.path.join(&global_config.generated_file);
    }
    fn env_filepath(&self, global_config: &GlobalConfig) -> Option<PathBuf> {
        let path = self.path.join(&global_config.provides_file);
        if path.exists() {
            Some(path)
        } else {
            None
        }
    }


    // todo: kind of ugly that this needs a mut ref
    fn write_generated_file(
        &mut self,
        contents: &str,
        global_config: &GlobalConfig,
    ) -> Result<(), io::Error> {
        if global_config.generated_file.is_absolute() {
            if global_config.generated_file.is_absolute() {
                let mut file = std::fs::OpenOptions::new()
                    .append(true)    // Only append to the file
                    .open(&global_config.generated_file)?;
        
                use std::io::Write;
                file.write_all(contents.as_bytes())?; // todo: link to source code
        
                debug!("Appended to {:#?}", global_config.generated_file);
                debug!("Appended {:#?}", contents);
            }
        } else {
            let file_path = self.generated_filepath(global_config);

            std::fs::write(&file_path, contents)?;
            self.sources.push(file_path.clone());

            debug!("Generated {:#?}", file_path);
            debug!("Generated {:#?}", contents);
            compile_to_zwc(&file_path)?;
        }
        Ok(())
    }

    fn write_env_file<'a>(&self, global_config: &'a GlobalConfig) -> Result<(), io::Error> {
        let mut file = None;
        for fun in self.fns.values() {
            if let Some(fstring) = &fun.fstring {
                if file.is_none() {
                    let file_path = &self.path.join(&global_config.provides_file);
                    file = Some(File::create(&file_path)?);
                    // todo: fix provision detection
                    if let Some(ref mut file) = file {
                        writeln!(file, "this={}", &self.get_alias_ref())?;
                        writeln!(file, "this_name={}", self.name)?;
                    }
                }
                if let Some(ref mut file) = file {
                    writeln!(file, "{}={}", fstring, fun.get_cmd(self, global_config))?;
                }
            }
        }
        // todo: delete obsolete env files? Shouldn't be an actual issue for now tho.
        // todo: compile
        Ok(())
    }
    fn env_contents<'a>(&self, global_config: &'a GlobalConfig) -> String {
        let mut contents = format!(
            "this={} this_name={} \\\n",
            &self.get_alias_ref(),
            self.name
        );
        for fun in self.fns.values() {
            if let Some(fstring) = &fun.fstring {
                contents.push_str(&format!(
                    "{}={} \\\n",
                    fstring,
                    fun.get_cmd(self, global_config)
                ));
            }
        }
        contents
        // todo: delete obsolete env files? Shouldn't be an actual issue for now tho.
        // todo: compile
    }

    // see plugin_from_dir
    fn is_proper<'a>(&self) -> bool {
        if let Some(alias) = &self.alias {
            alias != ""
        } else {
            true
        }
    }
}


fn compile_to_zwc(file_path: &PathBuf) -> Result<(), io::Error> {
    let output = Command::new("zsh")
    .arg("-c")
    .arg(format!("zcompile {}", file_path.to_string_lossy()))
    .output()?;

    if !output.status.success() {
        error!(
            "Compilation failed for {}: {}",
            file_path.to_string_lossy(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

fn write_generated_init_file<'a>(
    scanned_plugins: &'a HashMap<String, Plugin>,
    global_config: &GlobalConfig,
) -> Result<(), CreationError> {
    const FZS_INIT_ZSH: &[u8] = include_bytes!("../files/fzs_init.zsh");
    let mut contents = String::from_utf8_lossy(FZS_INIT_ZSH).to_string();
    let mut sorted_plugins: Vec<&Plugin> = scanned_plugins.values()
    .filter(|pg| pg.is_proper())
    .collect();
    sorted_plugins.sort_by(|a, b| a.name.cmp(&b.name));

    let mut replacements: HashMap<&str, Cow<'a, str>> = HashMap::new();
    let plugins_iter = sorted_plugins.into_iter();
    replacements.insert("fn_table", build_plugin_table(plugins_iter.clone(), &global_config).into());
    replacements.insert("all_fn_table", build_all_fn_table(plugins_iter, &global_config).into());

    contents = templatize_contents(contents, &global_config, &replacements)?;
    contents.push_str(
        &global_config.plugin_selector_binds.iter()
            .map(|kb| {
                format!(
                    "bindkey '{}' \"{}\"\n",
                    kb,
                    format!("{}.plugin-select.wg", global_config.fzs_name)
                )
            })
            .collect::<String>()
    );
    contents.push_str(
        &global_config.all_fn_selector_binds.iter()
            .map(|kb| {
                format!(
                    "bindkey '{}' \"{}\"\n",
                    kb,
                    format!("{}.all-fn-select.wg", global_config.fzs_name)
                )
            })
            .collect::<String>()
    );

    contents.push_str(&build_source_commands(
        scanned_plugins.values(),
        &global_config,
    ));
    fs::write(&global_config.init_file, contents)?;

    compile_to_zwc(&global_config.init_file)?;
    Ok(())
}

fn build_all_fn_table<'a>(plugins: impl Iterator<Item = &'a Plugin>, global_config: &'a GlobalConfig) -> String {
    let contents = plugins.flat_map(|pg| {
        pg.fns.values().filter(|fun| !fun.flags.contains(&FnFlag::NA) && !fun.flags.contains(&FnFlag::PG)).map(move |fun| {
            templatize(Some(fun), pg, &global_config.all_fn_table_template, &global_config, false, false)
        })
    });
    contents.collect::<Vec<_>>().join("\n")
}

fn build_source_commands<'a>(
    plugins: impl Iterator<Item = &'a Plugin>,
    global_config: &'a GlobalConfig,
) -> String {
    let mut source_content = String::new();
    for plugin in plugins {
        let mut sources = plugin
            .sources
            .iter()
            .map(|s| format!("\"{}\"", &pathbuf_to_string(s, global_config)))
            .collect::<Vec<String>>();
        sources.sort();

        if !sources.is_empty() {
            // todo: unsure if joining with cat is slower than calling source multiple times
            if sources.len() > 1 {
                source_content.push_str(&format!(
                    "\n{}source <(cat {})\n",
                    plugin.env_contents(global_config),
                    sources.join(" ")
                ));
            } else {
                source_content.push_str(&format!(
                    "\n{}source {}\n",
                    plugin.env_contents(global_config),
                    sources.get(0).unwrap()
                ));
            }

        }
    }
    source_content.push_str("this(){echo ${${funcstack[2]}%%.*};}\n");
    
    // source_content.push_str(&"\nunset fzs.temp");
    source_content
}

fn build_plugin_table<'a>(
    plugins: impl Iterator<Item = &'a Plugin>,
    global_config: &'a GlobalConfig,
) -> String {
    plugins
        .map(|pg| {
            format!(
                "{}\t{}\t{}\t{}\t{}",
                pg.get_alias_or_space(),
                pathbuf_to_string(&pg.path, global_config),
                templatize_simple(pg, &global_config.selector_widget_template),
                pg.name,
                pg.get_desc_ref().unwrap_or("")
            )
        })
        .collect::<Vec<String>>()
        .join("\n")
}

#[derive(Debug, Deserialize, Eq, PartialEq, Clone)]
struct Fun {
    name: String,
    bin: Option<PathBuf>,
    alias: Option<String>,
    #[serde(default)]
    desc: Option<String>,
    #[serde(default)]
    cmd: Option<String>,
    #[serde(default)]
    flags: FnFlags,
    #[serde(default)]
    binds: Keybinds,
    #[serde(default)]
    fstring: Option<String>,
}

// todo: safer flags
#[derive(Debug, Deserialize, PartialEq, Clone, Eq, Hash)]
enum FnFlag {
    WG, // Widget: Just makes selector invoke with zle. special as it doesn't override capability
    WR, // transforms target into a widget on shell and runs it
    WSUB, // transforms target into a pueue widget on shell
    WJR,
    WJSUB,
    PGI, // flatmap plugin
    PG, // Plugin
    PFN, // (Keeping this in in case we need to do more provisions)
    PBG, // Replace with run in background
    PE, // ProvideEnv
    SS, // Subshell
    RP, // RestorePrompt
    NC, // NoClean, what does this do again?
    NA, // NoAdd
    NR, // NR: fzf will just add cmd to buffer
    CMD, // Treat fstring as command
        // ForceSymlink?
    AL, // alias
    NN, // NoNamespace
}

// todo: fancier way?
// this overrides .to_string()?
impl fmt::Display for FnFlag {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let s = match self {
            FnFlag::WG => "WG",
            FnFlag::WR => "WR",
            FnFlag::WSUB => "WSUB",
            FnFlag::WJR => "WJR",
            FnFlag::WJSUB => "WJSUB",
            FnFlag::PG => "PG",
            FnFlag::PGI => "PGI",
            FnFlag::PFN => "PFN",
            FnFlag::PBG => "PBG",
            FnFlag::SS => "SS",
            FnFlag::RP => "RP",
            FnFlag::NC => "NC",
            FnFlag::NA => "NA",
            FnFlag::NR => "NR",
            FnFlag::PE => "PE",
            FnFlag::CMD => "CMD",
            FnFlag::AL => "AL",
            FnFlag::NN => "NN",
        };
        write!(f, "{}", s)
    }
}

impl FromStr for FnFlag {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_uppercase().as_str() {
            "WG" => Ok(FnFlag::WG),
            "WR" => Ok(FnFlag::WR),
            "WSUB" => Ok(FnFlag::WSUB),
            "WJR" => Ok(FnFlag::WJR),
            "WJSUB" => Ok(FnFlag::WJSUB),
            "PG" => Ok(FnFlag::PG),
            "PGI" => Ok(FnFlag::PFN),
            "PFN" => Ok(FnFlag::PFN),
            "PBG" => Ok(FnFlag::PBG),
            "SS" => Ok(FnFlag::SS),
            "RP" => Ok(FnFlag::RP),
            "NC" => Ok(FnFlag::NC),
            "NA" => Ok(FnFlag::NA),
            "NR" => Ok(FnFlag::NR),
            "PE" => Ok(FnFlag::PE),
            "CMD" => Ok(FnFlag::CMD),
            "AL" => Ok(FnFlag::AL),
            "NN" => Ok(FnFlag::NN),
            _ => Err(()),
        }
    }
}

impl FnFlag {
    fn cannot_on_script(&self) -> bool {
        self != &FnFlag::WG && (self.to_string().starts_with("W") || self.to_string().starts_with("P"))
    }
    fn is_widget(&self) -> bool {
        self.to_string().starts_with("W")
    }
}



impl Fun {
    fn merge_from(&mut self, other: Fun) {
        // Claude suggests self.alias = other.alias.clone().or(self.alias.take());
        if let Some(alias) = other.alias {
            self.alias = Some(alias);
        }
        if let Some(desc) = other.desc {
            self.desc = Some(desc);
        }

        self.flags = other.flags;
        if let Some(cmd) = other.cmd {
            warn!("cmd '{}' cannot be set on an existing Fn {}!", cmd, self.name)
        }
    }

    fn is_shell_function(&self) -> bool {
        self.flags
        .iter()
        .any(|item| item.to_string().starts_with("W") || item == &FnFlag::PG || item == &FnFlag::PBG )
    }

    fn does_provision(&self) -> bool {
        self.flags
        .iter()
        .any(|item| ( item.to_string().starts_with("W") || item.to_string().starts_with("P") ) && item != &FnFlag::PG && item != &FnFlag::WG )
    }

    // cannot have whitespace
    fn check_cmd_whitespace(&self) -> Result<(), ScanningError>  {
        if let Some(ref cmd) = self.cmd {
            if cmd.chars().any(char::is_whitespace) {
                return Err(ScanningError::InvalidFn(format!("Widget command for {} must be a valid identifier: Cannot contain whitespace", self.name)));
            }
        }
        Ok(())
    }

    fn check(&mut self) -> Result<(), ScanningError> {
        // self.is_shell_function() && !self.does_provision() && self.check_cmd_whitespace();
        let mut selector_flag = None;
        let mut is_widget = false;
        // responsible for ensuring sane run behavior when selected in plugin-selector
        for flag in self.flags.iter() {
            if flag == &FnFlag::PG {
                if self.alias.is_some() {
                    warn!("Aliases cannot be set on the plugin {}, removing", self.name);
                    self.alias = None;
                }
                if self.cmd.is_some() {
                    warn!("cmd cannot be set on the plugin {}, removing", self.name);
                    self.cmd = None;
                }
                selector_flag = Some(flag.clone());
                is_widget = true;
            } else if flag == &FnFlag::NR {
                if let Some(selector_flag) = selector_flag {
                    warn!("Function {} already has the run flag {} but another run flag {} was found, removing the new one.", self.name, selector_flag, flag);
                }
                selector_flag = Some(flag.clone());
            } else if flag.to_string().starts_with("W") {
                if let Some(selector_flag) = selector_flag {
                    warn!("Function {} already has the run flag {} but another run flag {} was found, removing the new one.", self.name, selector_flag, flag);
                }
                if flag == &FnFlag::WG { self.check_cmd_whitespace()?; }; // widget is unique in that it is called with zle, can have a command, and is not provisioned
                is_widget = true;
                selector_flag = Some(flag.clone());
            } else if flag == &FnFlag::SS {
                if let Some(selector_flag) = selector_flag {
                    warn!("Function {} already has the run flag {} but another run flag {} was found, removing the new one.", self.name, selector_flag, flag);
                }
                selector_flag = Some(flag.clone());
            }
        }
        if !is_widget && !self.binds.is_empty() {
            self.flags.insert(FnFlag::WG);
            info!("Treating {} as widget (WG) due to binds", self.name);
        }
        Ok(())
    }
    // used to template selector-plugins, and symlink files, since cmd cannot be set if path is this is ok
    fn get_cmd(&self, pg: &Plugin, global_config: &GlobalConfig) -> String {
        if pg.name == "base" || self.flags.contains(&FnFlag::NN) {
            self.name.clone()
        } else if self.flags.contains(&FnFlag::PG) {
            templatize_simple(self, &global_config.selector_widget_template)
        } else if self.does_provision() {
                    templatize(
                        self.into(),
                        pg,
                        &global_config.fn_template,
                        global_config,
                        true,
                        false
                    )
        } else {
            templatize(
                self.into(),
                pg,
                self.cmd.as_deref().unwrap_or(&global_config.fn_template), 
                global_config,
                true,
                false
            )
        }
    }

    fn get_real_cmd(&self, pg: &Plugin, global_config: &GlobalConfig) -> String {
        if pg.name == "base" || self.flags.contains(&FnFlag::NN) {
            self.name.clone()
        } else if self.flags.contains(&FnFlag::PG) {
            templatize_simple(self, &global_config.selector_widget_template)
        } else {
            templatize(
                self.into(),
                pg,
                self.cmd.as_deref().unwrap_or(&global_config.fn_template), 
                global_config,
                true,
                false
            )
        }
    }
    
    // todo: adapt for hashset
    // note this also allows plugin type unlike the flag method
    fn is_widget(&self) -> bool {
        self.flags
            .iter()
            .any(|item| item.is_widget() || item == &FnFlag::PG || item == &FnFlag::PBG )
    }
}

fn templatize_simple<T: Initial>(item: &T, s: &str) -> String {
    s.to_string()
        .replace("{{ name }}", item.get_name_ref())
        .replace("{{ alias }}", item.get_alias_ref())
        .replace("{{ desc }}", item.get_desc_ref().unwrap_or_default())
}

fn templatize(
    fun: Option<&Fun>,
    pg: &Plugin,
    s: &str,
    global_config: &GlobalConfig,
    simple: bool,
    alias_or_space: bool
) -> String {
    let mut res = s.to_string();

    let pg_alias = if alias_or_space {
        pg.get_alias_or_space()
    } else {
        pg.get_alias_ref()
    };
    
    res = res
        .replace("{{ pg_name }}", &pg.name)
        .replace("{{ pg_alias }}", &pg_alias);
    
    if let Some(fun) = fun {
        let fun_alias = if alias_or_space {
            fun.get_alias_or_space()
        } else {
            fun.get_alias_ref()
        };
    
        res = res
            .replace("{{ name }}", &fun.name)
            .replace("{{ alias }}", &fun_alias)
            .replace("{{ desc }}", &fun.desc.as_deref().unwrap_or(""));

        if !simple {
            res = res
                .replace("{{ cmds }}", &fun.get_cmd(pg, global_config))
                .replace(
                    "{{ flags }}",
                    &format!(
                        ",{},",
                        fun.flags
                            .iter()
                            .map(|flag| flag.to_string())
                            .collect::<Vec<String>>()
                            .join(",")
                    ),
                );
        }
    }
    res
}

type Keybinds = Vec<String>;
type FnFlags = HashSet<FnFlag>;

// type Wg = Fun;
// type Pl = Wg;

// #[derive(Debug, Deserialize)]
// struct Widget {
//     name: String,
//     #[serde(default)]
//     alias: Option<String>,
//     #[serde(default)]
//     cmd: Option<String>,
//     #[serde(default)]
//     desc: Option<String>,
// }

// impl Widget {
//     fn merge_from(&mut self, other: Widget) {
//         if let Some(alias) = other.alias {
//             self.alias = Some(alias);
//         }
//         if let Some(desc) = other.desc {
//             self.desc = Some(desc);
//         }
//         if let Some(cmd) = other.cmd {
//             self.cmd = Some(cmd);
//         }
//     }
// }

trait Initial {
    fn get_alias(&self) -> String;
    fn get_alias_ref(&self) -> &str;
    fn get_alias_or_space(&self) -> &str;
    fn get_name(&self) -> String;
    fn get_desc(&self) -> Option<String>;
    fn get_name_ref(&self) -> &String;
    fn get_desc_ref(&self) -> Option<&str>;
}

macro_rules! impl_initial {
    ($struct_name:ty) => {
        impl Initial for $struct_name {
            fn get_alias_ref(&self) -> &str {
                if self.alias.as_deref().map_or(true, |alias| alias.is_empty()) {
                    &self.name
                } else {
                    self.alias.as_deref().unwrap()
                }
            }

            fn get_alias_or_space(&self) -> &str {
                self.alias.as_deref().unwrap_or(" ")
            }

            fn get_alias(&self) -> String {
                if self.alias.as_deref().map_or(true, |alias| alias.is_empty()) {
                    self.name.clone()
                } else {
                    self.alias.clone().unwrap()
                }
            }
            fn get_name(&self) -> String {
                self.name.clone()
            }
            fn get_desc(&self) -> Option<String> {
                self.desc.clone()
            }
            fn get_name_ref(&self) -> &String {
                &self.name
            }
            fn get_desc_ref(&self) -> Option<&str> {
                self.desc.as_deref()
            }
        }
    };
}
impl_initial!(InitialPlugin);
impl_initial!(Fun);
impl_initial!(Plugin);
// impl_get_alias_with_fallback!(Wg, name, String);

impl InitialPlugin {
    fn to_plugin<'a>(self, fns: HashMap<String, Fun>) -> Plugin {
        Plugin {
            alias: self.alias,
            name: self.name,
            desc: self.desc,
            path: self.path,
            sources: Vec::new(),
            fns,
            fn_template: None,
            fn_table_template: None,
            binds: Keybinds::new(),
        }
    }
}

#[derive(Debug)]
struct GlobalConfig {
    root_dir: PathBuf,
    path_dir: PathBuf,
    config_dir: PathBuf,
    data_dir: PathBuf,
    plugin_regex: Regex,
    linkedbin_regex: Regex,
    fn_regex: Regex,
    name_from_cmd_regex: Regex,
    name_from_alias_template: String,
    selector_widget_template: String,
    fn_template: String,
    fn_table_template: String,
    all_fn_table_template: String,
    name_when_widget_template: String, // Unimplemented: probably we don't want this, rely on user to consistently name widgets
    template_file: PathBuf,
    init_file: PathBuf,
    fzs_name: String,
    generated_file: PathBuf,
    provides_file: PathBuf,
    plugin_selector_binds: Keybinds,
    all_fn_selector_binds: Keybinds,
    fzs_fzf_dir_cmd: String,
    fzs_fzf_pager_cmd: String,
    fzs_fzf_base_preview: String,
}

fn default_root_dir() -> Result<PathBuf, io::Error> {
    let home_dir = env::var("HOME").map_err(|e| io::Error::new(io::ErrorKind::NotFound, e))?;
    let root_dir = Path::new(&home_dir).join(".fzs");

    if !root_dir.exists() {
        fs::create_dir_all(&root_dir).map_err(|e| {
            io::Error::new(
                io::ErrorKind::Other,
                format!("Failed to create directory: {}", e),
            )
        })?;
    }
    Ok(root_dir)
}

fn default_path_dir() -> Result<PathBuf, io::Error> {
    let state_home =
        env::var("XDG_STATE_HOME").unwrap_or_else(|_| env::var("HOME").unwrap() + "/.local/state");

    let path_dir = Path::new(&state_home).join("fzs");

    if !path_dir.exists() {
        fs::create_dir_all(&path_dir).map_err(|e| {
            io::Error::new(
                io::ErrorKind::Other,
                format!("Failed to create state directory: {}", e),
            )
        })?;
    }
    Ok(path_dir)
}

fn config_dir(from: Option<String>) -> Result<PathBuf, io::Error> {
    let config_home = match from {
        Some(from) => PathBuf::from(from.replace("$HOME", &env::var("HOME").unwrap_or("$HOME".to_string()))),
        None => if let Ok(xdg) = env::var("XDG_CONFIG_HOME") {
            PathBuf::from(xdg).join("fzs")
        } else {
            PathBuf::from(env::var("HOME").unwrap()).join(".config").join("fzs")
        }
    };

    if !config_home.exists() {
        fs::create_dir_all(&config_home).map_err(|e| {
            io::Error::new(
                io::ErrorKind::Other,
                format!("Failed to create config directory: {}", e),
            )
        })?;
    }
    Ok(config_home)
}

fn data_dir(from: Option<String>) -> Result<PathBuf, io::Error> {
    let data_home = match from {
        Some(from) => PathBuf::from(from.replace("$HOME", &env::var("HOME").unwrap_or("$HOME".to_string()))),
        None => if let Ok(xdg) = env::var("XDG_DATA_HOME") {
            PathBuf::from(xdg).join("fzs")
        } else {
            PathBuf::from(env::var("HOME").unwrap()).join(".local").join("share").join("fzs")
        }
    };

    if !data_home.exists() {
        fs::create_dir_all(&data_home).map_err(|e| {
            io::Error::new(
                io::ErrorKind::Other,
                format!("Failed to create config directory: {}", e),
            )
        })?;
    }
    Ok(data_home)
}

#[derive(Debug, Deserialize)]
struct RawConfig {
    #[serde(default)]
    plugins: Vec<RawPlugin>,
    settings: RawGlobalConfig,
}
//
// merely stores compiled regexes
#[derive(Debug, Deserialize)]
struct RawGlobalConfig {
    root_dir: Option<String>,
    path_dir: Option<String>,
    data_dir: Option<String>,
    plugin_regex_str: Option<String>,
    linkedbin_regex_str: Option<String>,
    fn_regex_str: Option<String>,
    name_from_cmd_regex_str: Option<String>,
    name_from_alias_template: Option<String>,
    selector_widget_template: Option<String>,
    fn_template: Option<String>,
    fn_table_template: Option<String>,
    all_fn_table_template: Option<String>,
    name_when_widget_template: Option<String>,
    fzs_name: Option<String>,
    generated_file: Option<String>,
    provides_file: Option<String>,
    #[serde(default)]
    plugin_selector_binds: Option<Keybinds>,
    all_fn_selector_binds: Option<Keybinds>,
    fzf_dir_cmd: Option<String>,
    fzf_pager_cmd: Option<String>,
    fzf_base_preview: Option<String>
}

fn string_to_pathbuf(path: &str) -> PathBuf {
    PathBuf::from(path.replace("$HOME", &env::var("HOME").unwrap_or("$HOME".to_string())))
}

fn pathbuf_to_string(path: &PathBuf, global_config: &GlobalConfig) -> String {
    path.to_string_lossy()
        .replace(&global_config.path_dir.to_string_lossy().as_ref(), "$FZS_PATH_DIR")
        .replace(&global_config.root_dir.to_string_lossy().as_ref(), "$FZS_ROOT_DIR")
        .replace(&env::var("HOME").unwrap_or("$HOME".to_string()), "$HOME")
}

fn pathbuf_to_string_basic(path: &PathBuf) -> String {
    path.to_string_lossy()
        .replace(&env::var("HOME").unwrap_or("$HOME".to_string()), "$HOME")
}

impl RawGlobalConfig {
    fn to_global_config(self, config_dir: PathBuf) -> Result<GlobalConfig, io::Error> {
        let plugin_regex = Regex::new(&self.plugin_regex_str.unwrap_or(
            r"^([a-zA-Z0-9]+)(?:_([a-zA-Z0-9-]+))?(?:_([a-zA-Z0-9-]+))?_select$".to_string(),
        ))
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let fn_regex = Regex::new(
            &self.fn_regex_str.unwrap_or(
                r"^(_*[a-zA-Z0-9-]+)(?:_([a-zA-Z0-9-]*))?(?:_([a-zA-Z0-9-\(\)_ ]+))?(?:\.[a-zA-Z0-9,\. ]+)?$"
                    .to_string(),
            ), // allow _ in description
        )
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let linkedbin_regex = Regex::new(&self.linkedbin_regex_str.unwrap_or(
            r"^_([a-zA-Z0-9-]+)()(?:_([a-zA-Z0-9-]+))?(?:\.[a-zA-Z0-9\(\)\,\+\^]+)?$".to_string(),
        ))
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        let name_from_cmd_regex = Regex::new(
            &self
                .name_from_cmd_regex_str
                .unwrap_or(r"^(?:_*[a-zA-Z0-9-]+\.)?(.*)$".to_string()),
        )
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        let root_dir = match self.root_dir {
            Some(dir) => {
                let path = string_to_pathbuf(&dir);
                if !path.exists() {
                    fs::create_dir_all(&path).map_err(|e| {
                        io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            format!("Cannot create root_dir: {}", e),
                        )
                    })?;
                }
                // !path.metadata()?.permissions().readonly() doesn't work as expected
                if !path.is_dir() {
                    Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "root_dir is not writable or accessible",
                    ))?;
                }
                path
            }
            None => default_root_dir()?,
        };

        let path_dir = match self.path_dir {
            Some(dir) => {
                let path = string_to_pathbuf(&dir);
                if !path.exists() {
                    fs::create_dir_all(&path).map_err(|e| {
                        io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            format!("Cannot create path_dir: {}", e),
                        )
                    })?;
                }
                if !path.is_dir() || !path.metadata()?.permissions().readonly() {
                    Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "path_dir is not writable or accessible",
                    ))?;
                }
                path
            }
            None => default_path_dir()?,
        };

        let data_dir = if let Some(data_str) = self.data_dir {
            data_dir(Some(data_str))?
        } else {
            data_dir(None)?
        };


        let generated_file = match self.generated_file {
            Some(path) => string_to_pathbuf(&path),
            None => data_dir.join("fzs_plugins.zsh"), // Absolute path, recommend using uncommon extension like zsht if using relative paths
        };

        let provides_file = match self.provides_file {
            Some(path) => string_to_pathbuf(&path),
            None => PathBuf::from("fzs.env"),
        };

        let template_file = config_dir.join("template.zsh");

        let fzs_fzf_base_preview = self.fzf_base_preview.unwrap_or("source $fzs_init_file > /dev/null 2>&1; source $fzs_plugins_file > /dev/null 2>&1; which -a {3}".to_string());

        let plugin_selector_binds = self.plugin_selector_binds.unwrap_or(vec!["^[p".to_string()]);
        let all_fn_selector_binds = self.all_fn_selector_binds.unwrap_or(vec!["^[f".to_string()]);

        let init_file = data_dir.join("fzs_init.zsh");
        let gc = GlobalConfig {
            root_dir,
            path_dir,
            config_dir,
            data_dir,
            plugin_regex,
            fn_regex,
            linkedbin_regex,
            name_from_cmd_regex,
            name_from_alias_template: self.name_from_alias_template.unwrap_or("{{ alias }}.al".to_string()),
            selector_widget_template: self
                .selector_widget_template
                .unwrap_or("{{ alias }}._select.wg".to_string()),
            fn_template: self
                .fn_template
                .unwrap_or("{{ pg_alias }}.{{ name }}".to_string()),
            name_when_widget_template: self
                .name_when_widget_template
                .unwrap_or("{{ name }}.wg".to_string()),
            fn_table_template: self
                .fn_table_template
                .unwrap_or("{{ name }}	{{ flags }}	{{ cmds }}	{{ desc }}".to_string()),
            all_fn_table_template: self
                .all_fn_table_template
                .unwrap_or("{{ pg_alias }}		{{ cmds }}	{{ name }}		{{ alias }}		{{ desc }}".to_string()),
            template_file,
            init_file,
            fzs_name: self.fzs_name.unwrap_or("fzs".to_string()),
            generated_file,
            provides_file,
            plugin_selector_binds,
            all_fn_selector_binds,
            fzs_fzf_dir_cmd: self.fzf_dir_cmd.unwrap_or("ls -la".to_string()),
            fzs_fzf_pager_cmd: self.fzf_pager_cmd.unwrap_or("less -RX".to_string()),
            fzs_fzf_base_preview
        };

        const TEMPLATE_ZSH: &[u8] = include_bytes!("../files/template.zsh");
        if !gc.template_file.exists() {
            fs::write(&gc.template_file, TEMPLATE_ZSH)?;
        }

        Ok(gc)
    }
}

// todo: Or should we use IterStruct?
// https://www.reddit.com/r/learnrust/comments/z37jm8/how_to_iterate_over_struct_fieldvariable_names/?

#[derive(Debug, Deserialize)]
struct RawPlugin {
    name: String,
    desc: Option<String>,
    alias: Option<String>,
    #[serde(default)]
    fns: Vec<Fun>,
    // these last fields are wrapped with Option to allow default from Global when parsed into Plugin
    fn_template: Option<String>,
    #[serde(default)]
    binds: Keybinds,
}

impl Plugin {
    fn merge_from_raw(&mut self, raw_plugin: RawPlugin) -> Result<(), ScanningError> {
        if let Some(desc) = raw_plugin.desc {
            self.desc = Some(desc);
        }

        if let Some(alias) = raw_plugin.alias {
            self.alias = Some(alias);
        }

        self.binds = raw_plugin.binds;

        if let Some(fn_template) = raw_plugin.fn_template {
            self.fn_template = fn_template.into();
        }

        // Merge fns
        for mut raw_fn in raw_plugin.fns {
            if let Some(existing_fn) = self.fns.get_mut(&raw_fn.name) {
                existing_fn.merge_from(raw_fn);
                existing_fn.check()?;
            } else {
                raw_fn.check()?;
                self.fns.insert(raw_fn.name.clone(), raw_fn.into());
            }
        }
        Ok(())
    }
    fn map_includes(&mut self, plugins: HashMap<String, Plugin>) -> Result<(), ScanningError> {
        let mut includes=Vec::new();
        self.fns.retain(|_, fun| {
            if fun.flags.contains(&FnFlag::PGI) {
                includes.push(fun.name.clone());
                false
            } else {
                true
            }
        });
        for pg_key in includes {
            if let Some(pg) = plugins.get(&pg_key) {
                for (name,fun) in &pg.fns {
                    if self.fns.contains_key(name) {
                        return Err(ScanningError::DuplicateFunctionName(
                            name.to_string(),
                            format!("Attempted to include {} into {}",pg_key,self.name)
                        ));
                    }
                    self.fns.insert(name.clone(), fun.clone());
                }
            } else {
                warn!("Plugin {} not found in scanned plugins.", pg_key);
            }
        };
        Ok(())
    }
}

fn plugin_from_dir(
    path: &Path,
    plugin_regex: &Regex,
    linkedbin_regex: &Regex,
) -> Result<Scanned, ScanningError> {
    let dir_name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Invalid directory name"))?
        .to_string_lossy();

    if let Some(caps) = plugin_regex.captures(&dir_name) {
        let name = caps
            .get(1)
            .map_or_else(|| "".to_string(), |m| m.as_str().to_string());

        let alias = caps.get(2).and_then(|m| {
            let s = m.as_str().to_string();
            if s.is_empty() {
                None
            } else {
                Some(s)
            }
        });

        let desc = caps.get(3).map(|m| m.as_str().to_string());

        let plugin = InitialPlugin {
            name,
            path: path.to_path_buf(),
            alias,
            desc,
        };

        Ok(Scanned::ScannedPlugin(plugin))
    } else if let Some(caps) = linkedbin_regex.captures(&dir_name) {
        let name = caps
            .get(1)
            .map_or_else(|| "".to_string(), |m| m.as_str().to_string());
        let alias = Some("".to_string()); // always empty
        let desc = caps.get(3).map(|m| m.as_str().to_string());

        let initial_plugin = InitialPlugin {
            name,
            path: path.to_path_buf(),
            alias,
            desc,
        };
        Ok(Scanned::ScannedLinkedbin(initial_plugin))
    } else {
        Ok(Scanned::None)
    }
}

// used a generic function pointer for learning purpose
fn scan_for_plugins(
    root_dir: &Path,
    plugin_regex: &Regex,
    linkedbin_regex: &Regex,
) -> Result<(HashMap<String, InitialPlugin>, Vec<InitialPlugin>), ScanningError> {
    let mut plugins = HashMap::new();
    let mut linkedbins = Vec::new();

    // Recursively scan directories
    for entry in fs::read_dir(root_dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.is_dir() {
            // Scan the directory
            match plugin_from_dir(&path, plugin_regex, linkedbin_regex)? {
                Scanned::ScannedPlugin(plugin) => {
                    if plugins.contains_key(&plugin.name) {
                        error!("Plugins hashmap: {:#?}", plugins);
                        return Err(ScanningError::DuplicatePluginIdentifier(
                            plugin.name.clone(),
                        ));
                    } else {
                        plugins.insert(plugin.name.clone(), plugin);
                    }
                }
                Scanned::ScannedLinkedbin(linkedbin) => linkedbins.push(linkedbin),
                Scanned::None => (),
            };

            let (sub_plugins, sub_linkedbins) =
                scan_for_plugins(&path, plugin_regex, linkedbin_regex)?;
            plugins.extend(sub_plugins);
            linkedbins.extend(sub_linkedbins);
        }
    }

    Ok((plugins, linkedbins))
}

fn process_cmd(
    name: String,
    path: Option<&PathBuf>,
    alias: Option<String>,
    desc: Option<String>,
    cmd: String,
    flags: FnFlags,
    binds: Keybinds,
    fns: &mut HashMap<String, Fun>,
    global_config: &GlobalConfig,
) -> Result<(), ScanningError> {
    if fns.contains_key(&name) {
        return Err(ScanningError::DuplicateFunctionName(
            name,
            path.map_or(cmd, |p| pathbuf_to_string(p, global_config)),
        ));
    }
    let fun = Fun {
        name: name.clone(),
        bin: None,
        alias,
        flags,
        binds,
        cmd: Some(cmd),
        desc,
        fstring: None,
    };
    fns.insert(name, fun);

    Ok(())
}

fn process_fstring(
    fstring: &str,
    path: Option<&PathBuf>,
    flags: FnFlags,
    binds: Vec<String>,
    fns: &mut HashMap<String, Fun>,
    global_config: &GlobalConfig,
    store_fstring: bool,
) -> Result<(), ScanningError> {
    if let Some(caps) = global_config.fn_regex.captures(fstring) {
        let name = caps.get(1).map(|m| m.as_str().to_string()).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "No name captured from file name, fn_regex should have at least one capturing group.", // todo: move this check out. But is there a way for Rust to know that a capture group exists at compile time?
            )
        })?;

        let alias = caps.get(2).and_then(|m| {
            let s = m.as_str().to_string();
            if s.is_empty() {
                None
            } else {
                Some(s)
            }
        });

        let desc = caps.get(3).and_then(|m| {
            let s = m.as_str().to_string();
            if s.is_empty() {
                None
            } else {
                Some(s)
            }
        });

        let fstring = if store_fstring {
            Some(fstring.to_string())
        } else {
            None
        };

        if fns.contains_key(&name) {
            return Err(ScanningError::DuplicateFunctionName(
                name,
                path.map_or(
                    fstring.unwrap_or("Neither fstring nor path available".to_string()),
                    |p| p.to_string_lossy().into_owned(),
                ),
            ));
        }
        let fun = Fun {
            name: name.clone(),
            bin: path.cloned(), // todo: investigate these functors more
            alias,
            desc,
            flags,
            cmd: None,
            binds,
            fstring: fstring.clone(),
        };
        fns.insert(name.clone(), fun);
    } else {
        if !path.is_none() {
            warn!(
                "File name does not match regex: {}",
                path.unwrap().display()
            )
        };
    }
    Ok(())
}

fn recurse_files(dir: &Path, files: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.is_dir() {
            recurse_files(&path, files)?;
        } else if path.is_file() {
            files.push(path);
        }
    }
    Ok(())
}

fn populate_plugins<'a>(
    plugins: &mut HashMap<String, Plugin>,
    to_parse: impl Iterator<Item = InitialPlugin>,
    default_flags: &FnFlags,
    provide_envs: &mut HashMap<String, (String, i32)>, // fstring -> plugin -> function
    global_config: &'a GlobalConfig,
) -> Result<(), ScanningError> {
    for ip in to_parse {
        let ip_clone = ip.clone();
        let pg = plugins
            .entry(ip_clone.name.clone())
            .or_insert(ip.to_plugin(HashMap::new()));
        let fns = &mut pg.fns;

        // special base plugin is fully recursive
        let files = if pg.name == "base" {
            let mut files = Vec::new();
            recurse_files(&ip_clone.path, &mut files)?;
            files
        } else {
            fs::read_dir(&ip_clone.path)?
                .filter_map(|entry| entry.ok())
                .map(|e| e.path())
                .collect::<Vec<_>>()
        };

        for path in files {
            if path.is_file() && path.is_executable() {
                let fname = path
                    .file_name()
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Invalid file name"))?
                    .to_string_lossy();

                // process flags
                let (fstring, ext): (&str, Option<&str>) = match fname.split_once('.') {
                    Some((fstring, ext)) => (fstring, Some(ext)),
                    None => (&fname, None),
                };

                let (e_name, e_alias, e_desc, mut e_flags, e_binds, _) = process_ext(ext);
                e_flags.extend(default_flags.iter().cloned());
                // this is useless but we have it for compatibility?
                if e_flags.contains(&FnFlag::CMD) {
                    let name = e_name.unwrap_or(match global_config.name_from_cmd_regex.captures(fstring) {
                        Some(caps) => caps
                            .get(1)
                            .map(|m| m.as_str().to_string())
                            .unwrap_or(fstring.to_string()),
                        None => fstring.to_string(),
                    });
                    process_cmd(
                        name,
                        Some(&path),
                        e_alias,
                        e_desc,
                        fstring.to_string(),
                        e_flags,
                        e_binds,
                        fns,
                        &global_config,
                    )?;
                } else {
                    process_fstring(
                        &fstring,
                        Some(&path),
                        e_flags,
                        e_binds,
                        fns,
                        &global_config,
                        false,
                    )?;
                }
            } else {
                if let Some(basename) = path.file_name() {
                    if let Some(basename_str) = basename.to_str() {
                        if let Some(basename_str) = basename_str.strip_suffix(".zshrc") {
                            let mut file_flags = FnFlags::new();
                            if let Some(pos) = basename_str.rfind('.') {
                                parse_file_flags(&basename_str[pos + 1..], &mut file_flags)
                            }
                            debug!("Populating from {}", &path.display());
                            match populate_from_file(
                                &path,
                                fns,
                                provide_envs,
                                &ip_clone,
                                file_flags,
                                global_config,
                            ) {
                                Ok(_) => pg.sources.push(path),
                                Err(err) => error!("Failed to parse {}: {}", path.display(), err),
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

fn parse_file_flags(val: &str, flags: &mut FnFlags) {
    for flag in val.split(',') {
        match FnFlag::from_str(flag) {
            Ok(parsed_flag) => {
                flags.insert(parsed_flag);
            }
            Err(_) => {
                if !flags.is_empty() {
                    warn!("Encountered an invalid flag {} in {}", flag, val);
                }
            }
        }
    }
}

fn process_ext(ext: Option<&str>) -> (Option<String>, Option<String>, Option<String>, FnFlags, Keybinds, Option<String>) {
    let mut name = None;
    let mut alias = None;
    let mut flags = FnFlags::new();
    let mut binds = Keybinds::new(); // assuming binds is a Vec or similar structure
    let mut capturing_desc = false;
    let mut desc_parts = Vec::new();
    let mut cmd = None;

    if let Some(ext_string) = ext {
        for word in ext_string.split_whitespace() {
            if capturing_desc {
                desc_parts.push(word.to_string());
                continue;
            }
            if let Some(val) = word.strip_prefix("name=") {
                name = Some(val.to_string());
            } else if let Some(val) = word.strip_prefix("alias=") {
                alias = Some(val.to_string());
            } else if let Some(val) = word.strip_prefix("desc=") {
                capturing_desc = true;
                desc_parts.push(val.to_string());
            } else if let Some(val) = word.strip_prefix("binds=") {
                for bind in val.split(',') {
                    binds.push(bind.to_string());
                }
            } else if let Some(val) = word.strip_prefix("cmd=") {
                flags.insert(FnFlag::CMD);
                    name = name.or_else(|| {
                        Some(
                            val.chars()
                                .filter(|c| c.is_alphanumeric() || *c == '_')
                                .collect::<String>(),
                        )
                    });
                cmd = Some(val.to_string());
            } else if let Some(_) = word.strip_prefix(".") {
                () // ignore extensions like .zsh
            } else {
                let val = word.strip_prefix("flags=").unwrap_or(word);
                if !flags.is_empty() {
                    warn!("Flags (unprefixed words) defined twice in {}, skipping.", ext_string);
                } else {
                    for flag in val.split(',') {
                        match FnFlag::from_str(flag) { // potentially we want to filter valid?
                            Ok(parsed_flag) => {
                                flags.insert(parsed_flag);
                            },
                            Err(_) => {
                                if !flags.is_empty() {
                                    warn!("Encountered an invalid flag in {}", ext_string);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    let desc = if !desc_parts.is_empty() {
        Some(desc_parts.join(" "))
    } else {
        None
    };

    (name, alias, desc, flags, binds, cmd)
}

// todo: use regex for safer substitution
fn replace_shell<T: Initial>(cmd_string: &str, pg: &T) -> String {
    cmd_string
        .replace("${this}", pg.get_alias_ref())
        .replace("${this_name}", pg.get_name_ref())
        .replace("$this", pg.get_alias_ref())
        .replace("$this_name", pg.get_name_ref())
}

fn populate_from_file<T: Initial>(
    file_path: &PathBuf,
    fns: &mut HashMap<String, Fun>,
    provide_envs: &mut HashMap<String, (String, i32)>,
    pg: &T,
    file_flags: FnFlags,
    global_config: &GlobalConfig,
) -> Result<(), ScanningError> {
    let file = File::open(file_path)?;
    let reader = BufReader::new(file);

    let mut parse_next = false;
    let mut flags = FnFlags::new();
    let mut binds = Vec::new();
    let mut e_name = None;
    let mut e_alias = None;
    let mut e_desc = None;
    let mut e_cmd: Option<String>;

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                error!("Error reading line: {}", e);
                "".to_string()
            }
        };
        let pattern = "# :";

        let func_pattern = regex::Regex::new(r"(?:^| )\$([a-zA-Z0-9_]+)\s*\(").unwrap(); // https://stackoverflow.com/questions/2821043/allowed-characters-in-linux-environment-variable-names
        let cmd_pattern = regex::Regex::new(r"(?:^| )([\$a-zA-Z0-9_.\-\&]+)\s*\(").unwrap();
        let alias_pattern = regex::Regex::new(r"alias ([\$a-zA-Z0-9_.\-\&]+)=").unwrap();

        if let Some(directives) = line.trim_start().strip_prefix(&pattern) {
            (e_name, e_alias, e_desc, flags, binds, e_cmd) = process_ext(Some(directives));
            if flags.contains(&FnFlag::PG) {
                debug!("found plugin {}", &directives);
                if let Some(name) = e_name {
                    if fns.contains_key(&name) {
                        return Err(ScanningError::DuplicateFunctionName(
                            name,
                            pathbuf_to_string(file_path, global_config)),
                        );
                    }
                    let fun = Fun {
                                name: name.clone(),
                                alias: None,
                                desc: e_desc,
                                flags,
                                cmd: None,
                                binds,
                                bin: None,
                                fstring: None
                            };
                    fns.insert(name.clone(), fun);
                } else {
                    warn!("Encountered PG declaration without a name in {}, skipping", pathbuf_to_string(file_path, global_config));
                }
                flags = FnFlags::new();
                binds = Keybinds::new();
                e_alias = None; // compiler needs help
                e_name= None;
                e_desc=None;
            } else if let Some(cmd) = e_cmd {
                debug!("found cmd {}", &directives);
                process_cmd(
                            e_name.unwrap(),
                            Some(file_path),
                            e_alias,
                            e_desc,
                            cmd,
                            flags,
                            binds,
                            fns,
                            global_config,
                        )?;
                flags = FnFlags::new();
                binds = Keybinds::new();
                e_alias = None; // compiler needs help
                e_name= None;
                e_desc=None;
            }
            else {
                parse_next=true;
            }
        } else if parse_next {
            if line.trim_start().starts_with("#") || line.trim().is_empty() {
                continue;
            };
            parse_next = false;
            flags.extend(file_flags.clone());
            if flags.contains(&FnFlag::CMD) {
                if let Some(caps) = cmd_pattern.captures(&line) {
                    if let Some(cstring) = caps.get(1) {
                        flags = flags.into_iter().filter(|flag| if flag.cannot_on_script() { warn!("Flag {:?} cannot decorate a function ({}), skipping", flag, file_path.display()); false } else {true}).collect();
                        // todo: check namespace or command has no whitespace before allowing WG flag
                        let cstring=replace_shell(cstring.into(), pg);
                        debug!("found cstring {}", &cstring);
                        let name = e_name.unwrap_or(match global_config.name_from_cmd_regex.captures(&cstring) {
                            Some(caps) => caps
                                .get(1)
                                .map(|m| m.as_str().to_string())
                                .unwrap_or(cstring.to_string()),
                            None => cstring.to_string(),
                        });
                        process_cmd(
                            name,
                            Some(file_path),
                            e_alias,
                            e_desc,
                            cstring.to_string(),
                            flags,
                            binds,
                            fns,
                            &global_config,
                        )?;
                    }
                }
                
            }   else {
                if flags.contains(&FnFlag::AL) {
                    if let Some(caps) = alias_pattern.captures(&line) {
                        if let Some(cstring) = caps.get(1) {
                            let alias=replace_shell(cstring.into(), pg);
                            debug!("found alias {}", &alias);
                            let name= global_config.name_from_alias_template.replace("{{ alias }}", &alias.as_str());
    
                            if fns.contains_key(&name) {
                                return Err(ScanningError::DuplicateFunctionName(
                                    name,
                                    line
                                ));
                            }
                            let fun = Fun {
                                name: name.clone(),
                                alias: alias.as_str().to_string().into(),
                                desc: e_desc,
                                flags,
                                cmd: alias.as_str().to_string().into(),
                                binds,
                                bin: None,
                                fstring: None
                            };
                            fns.insert(name.clone(), fun);
                        }
                    }
                }   else if let Some(caps) = func_pattern.captures(&line) {
                    flags = flags.into_iter().filter(|flag| if flag.cannot_on_script() { warn!("Flag {:?} cannot decorate a function ({}), skipping", flag, file_path.display()); false } else {true}).collect();
                    if let Some(cstring) = caps.get(1) {
                        let fstring=replace_shell(cstring.into(), pg);
                        debug!("found fstring {}", &fstring.as_str());
                        process_fstring(
                            fstring.as_str(),
                            None,
                            flags,
                            binds,
                            fns,
                            global_config,
                            true,
                        )?;
                    }
                } else {
                    warn!(
                        "No match found following a declared line: {} in {}",
                        line,
                        file_path.display()
                    );
                }
            }
            flags = FnFlags::new();
            binds = Keybinds::new();
            e_name = None;
            e_alias = None;
            e_desc= None;
        }
    }
    Ok(())
}

fn symlink_fns<'a>(
    plugins: impl Iterator<Item = &'a Plugin>,
    global_config: &GlobalConfig,
) -> Result<(), CreationError> {
    // backup+clean symlinks
    let temp_symlink_dir = Builder::new().tempdir_in(&global_config.path_dir)?;
    debug!("TempDir: {:?}", temp_symlink_dir);
    for entry in fs::read_dir(&global_config.path_dir)? {
        let entry = entry?;
        let path = entry.path();

        if fs::symlink_metadata(&path)?.file_type().is_symlink() {
            let file_name = path.file_name().unwrap();
            let temp_path = temp_symlink_dir.path().join(file_name);

            fs::rename(&path, &temp_path)?;
        }
    }

    // Symlink all executables associated with the plugins to the `path_dir` directory, using the appropriate naming scheme.
    for pg in plugins {
        for fun in pg.fns.values() {
            if !fun.flags.contains(&FnFlag::WG) && !fun.flags.contains(&FnFlag::PG) {
                if let Some(source_path) = &fun.bin {
                    let symlink_path = global_config.path_dir.join(fun.get_cmd(pg, global_config));
                    if let Err(e) = {
                        debug!(
                            "Symlinking {} -> {}",
                            &source_path.display(),
                            &symlink_path.display()
                        );
                        symlink_file(&source_path, &symlink_path)
                    } {
                        restore_symlinks(temp_symlink_dir.path(), &global_config.path_dir)?; // todo: don't early exit
                        return Err(CreationError::SymlinkError(
                            source_path.clone(),
                            symlink_path.clone(),
                            e.to_string(),
                        ));
                    }
                }
            }
        }
    }

    Ok(())
}

fn templatize_contents<'a>(
    mut contents: String,
    global_config: &'a GlobalConfig,
    replacements: &HashMap<&str, Cow<'a, str>>
) -> Result<String, std::io::Error> {
    for (key, replacement) in replacements {
        contents = contents.replace(&format!("{{{{ {} }}}}", key), replacement);
    }
    contents = contents.replace(
        &format!("{{{{ {} }}}}", "fzs_name"),
        &global_config.fzs_name,
    ).replace(
        &format!("{{{{ {} }}}}", "fzs_root_dir"),
        &pathbuf_to_string_basic(&global_config.root_dir),
    ).replace(
        &format!("{{{{ {} }}}}", "fzs_path_dir"),
        &pathbuf_to_string_basic(&global_config.path_dir),
    ).replace(
        &format!("{{{{ {} }}}}", "fzs_data_dir"),
        &pathbuf_to_string_basic(&global_config.data_dir),
    ).replace(
        &format!("{{{{ {} }}}}", "fzs_config_dir"),
        &pathbuf_to_string_basic(&global_config.config_dir),
    ).replace(
        &format!("{{{{ {} }}}}", "fzs_provides_file"),
        &pathbuf_to_string(&global_config.provides_file, &global_config),
    ).replace(
        &format!("{{{{ {} }}}}", "fzs_fzf_dir_cmd"),
        &global_config.fzs_fzf_dir_cmd,
    ).replace(
        &format!("{{{{ {} }}}}", "fzs_fzf_pager_cmd"),
        &global_config.fzs_fzf_pager_cmd,
    ).replace(
        &format!("{{{{ {} }}}}", "fzs_init_file"),
        &pathbuf_to_string(&global_config.init_file,&global_config),
    ).replace(
        &format!("{{{{ {} }}}}", "fzs_fzf_base_preview"),
        &global_config.fzs_fzf_base_preview,
    );
    if global_config.generated_file.is_absolute() {
        contents = contents.replace(        &format!("{{{{ {} }}}}", "fzs_plugins_file"),
        &pathbuf_to_string(&global_config.generated_file, &global_config))
    }
    Ok(contents)
}



// todo: optimize
fn templatize_plugins<'a>(
    scanned_plugins: &'a mut HashMap<String, Plugin>,
    global_config: &'a GlobalConfig,
) -> Result<(), std::io::Error> {
    if global_config.generated_file.is_absolute() {
        if let Err(e) = fs::write(&global_config.generated_file, &[]) {
            eprintln!("Failed to clear the file: {}", e);
        }
    }
    for plugin in scanned_plugins.values_mut() {
        let mut replacements: HashMap<&str, Cow<'a, str>> = HashMap::new();
        let mut contents = "".to_string();

        if let Some(fn_table) = plugin.fn_table(global_config) {
            contents = fs::read_to_string(&global_config.template_file)?;
            replacements.insert("fn_table".into(), fn_table.into());
            replacements.insert(
                "selector_name".into(),
                templatize_simple(plugin, &global_config.selector_widget_template).into(),
            );
            contents = templatize_contents(contents, global_config, &replacements)?;
        }

        if let Some(extra_table) = plugin.extra_table(global_config) {
            contents.push_str("\n### ALIASES AND BINDS\n");
            contents.push_str(&extra_table);
        }
        if !contents.is_empty() {
            plugin.write_generated_file(&contents, global_config)?;
        }
    }
    if global_config.generated_file.is_absolute() {
        compile_to_zwc(&global_config.generated_file)?;
    }
    Ok(())
}

// passed iterators by value as they are "used up"
fn write_envs<'a>(
    scanned_plugins: impl Iterator<Item = &'a Plugin>,
    global_config: &'a GlobalConfig,
) -> Result<(), std::io::Error> {
    for plugin in scanned_plugins {
        plugin.write_env_file(global_config)?;
    }
    Ok(())
}

fn restore_symlinks(temp_dir: &Path, target_dir: &Path) -> io::Result<()> {
    for entry in fs::read_dir(temp_dir)? {
        let entry = entry?;
        let temp_path = entry.path();
        let file_name = temp_path.file_name().unwrap();
        let target_path = target_dir.join(file_name);

        if let Err(e) = {
            if target_path.exists() {
                fs::remove_file(&target_path)?;
            }
            fs::rename(&temp_path, &target_path)
        } {
            eprintln!("Failed to restore symlink {}: {}", target_path.display(), e);
        }
    }
    Ok(())
}

// should these use refs? Seems not worth it to differentiate?
#[derive(Debug, Display)]
enum ScanningError {
    #[display("Duplicate function name found: {}. Info: {}", _0, _1)]
    DuplicateFunctionName(String, String),
    #[display("Duplicate plugin name or alias found: {}", _0)]
    DuplicatePluginIdentifier(String),
    #[display("Invalid fn: {}", _0)]
    InvalidFn(String),
    #[display("fn {} is declared as a reference to a nonexistent plugin {}.", _0, _1)]
    MissingPlugin(String, String),
    #[display("IO error: {}", _0)]
    Io(io::Error),
}

#[derive(Debug, Display)]
enum CreationError {
    #[display("Couldn't create symlink: {:?} -> {:?}. Info: {}", _0, _1, _2)]
    SymlinkError(PathBuf, PathBuf, String),
    #[display("IO error: {}", _0)]
    Io(io::Error),
}

#[derive(Debug, Display)]
enum FzsErrors {
    Scanning(ScanningError),
    Creation(CreationError),
    #[display("IO error: {}", _0)]
    Io(io::Error),
    // ?: required for `Result<(), FzsErrors>` to implement `FromResidual<Result<Infallible, OsString>>
    #[display("OSString error: {:?}", _0)]
    OsString(OsString),
}

macro_rules! impl_from_error_enum {
    ($from:ty, $to:ty, $to_subtype:path) => {
        impl From<$from> for $to {
            fn from(err: $from) -> $to {
                $to_subtype(err)
            }
        }
    };
}
impl_from_error_enum!(io::Error, ScanningError, ScanningError::Io);
impl_from_error_enum!(io::Error, CreationError, CreationError::Io);

impl_from_error_enum!(ScanningError, FzsErrors, FzsErrors::Scanning);
impl_from_error_enum!(CreationError, FzsErrors, FzsErrors::Creation);
impl_from_error_enum!(io::Error, FzsErrors, FzsErrors::Io);
impl_from_error_enum!(OsString, FzsErrors, FzsErrors::OsString);

enum Scanned {
    ScannedPlugin(InitialPlugin),
    ScannedLinkedbin(InitialPlugin),
    None,
}

fn finalize_plugins<'a>(
    plugins: &mut HashMap<String, Plugin>,
    global_config: &'a GlobalConfig,
) -> Result<(), ScanningError> {
    let mut q: Vec<(String, String, String)> = Vec::new();
    for pg in plugins.values() {
        for fun in pg.fns.values() {
            if fun.flags.contains(&FnFlag::PG) {
                if let Some(plugin) = plugins.get(&fun.name) {
                    q.push((pg.name.to_string(), fun.name.to_string(), plugin.get_alias()));  // Assuming alias is an Option and plugin is clonable
                } else {
                    return Err(ScanningError::MissingPlugin(pg.name.to_string(), fun.name.to_string()));
                }
            }
        }
    }
    for (pg, fun, alias) in q {
        plugins.get_mut(&pg).unwrap().fns.get_mut(&fun).unwrap().alias = Some(alias);
    }
    Ok(())
}
fn main() -> Result<(), FzsErrors> {
    env_logger::Builder::from_env(Env::default().default_filter_or("warn")).init();
    let config_dir = config_dir(None)?; // Use the config_dir function to get the directory path
    let config_file_path = config_dir.join("config.toml"); // Append the config.toml file to the path

    debug!("{:#?}", config_file_path);

    let toml_content = fs::read_to_string(config_file_path).map_err(|e| {
        io::Error::new(
            io::ErrorKind::Other,
            format!("Failed to read config.toml: {}", e),
        )
    })?;

    let raw_config: RawConfig =
        toml::from_str(&toml_content).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    debug!("Raw Config {:#?}", &raw_config);

    let raw_global_config = raw_config.settings;
    let global_config = raw_global_config.to_global_config(config_dir)?;
    debug!("Global Config {:#?}", &global_config);

    let raw_plugins: Vec<RawPlugin> = raw_config.plugins;

    let (scanned_initial_plugins, scanned_initial_linkedbins) = scan_for_plugins(
        &global_config.root_dir,
        &global_config.plugin_regex,
        &global_config.linkedbin_regex,
    )?;

    let mut provide_envs = HashMap::new(); //currently unimplemented

    let mut scanned_plugins = HashMap::new();
    populate_plugins(
        &mut scanned_plugins,
        scanned_initial_plugins.into_values(),
        &FnFlags::new(),
        &mut provide_envs,
        &global_config,
    )?;
    populate_plugins(
        &mut scanned_plugins,
        scanned_initial_linkedbins.into_iter(),
        &HashSet::from([FnFlag::NA]),
        &mut provide_envs,
        &global_config,
    )?;

    for rp in raw_plugins {
        match scanned_plugins.get_mut(&rp.name) {
            Some(plugin) => {
                plugin.merge_from_raw(rp)?;
            }
            None => {
                warn!("Plugin {} not found in scanned plugins.", rp.name);
                info!("Plugins from config haven't been implemented yet, try an empty folder");
                // scanned_plugins.insert(rp.name, rp.into());
            }
        }
    }

    finalize_plugins(&mut scanned_plugins, &global_config)?;

    debug!("Scanned Plugins {:#?}", scanned_plugins);

    templatize_plugins(&mut scanned_plugins, &global_config)?;
    // write_envs(scanned_plugins.values(), &global_config)?;
    symlink_fns(scanned_plugins.values(), &global_config)?;


    let home_dir = env::var("HOME").unwrap_or_else(|_| String::from("~"));
    write_generated_init_file(&scanned_plugins, &global_config)?;

    eprintln!("All operations complete! Run the following code to add the initialization step to your .zshrc if you haven't already.");

    eprintln!(
        "echo '. \"{}\"' >> ~/.zshrc",
        global_config
            .init_file
            .into_os_string()
            .into_string()?
            .replace(&home_dir, &"~")
    );
    if global_config.generated_file.is_absolute() {
        eprintln!(
            "echo '. \"{}\"' >> ~/.zshrc",
            global_config
                .generated_file
                .into_os_string()
                .into_string()?
                .replace(&home_dir, &"~")
        );
    }
    Ok(())
}
