use libloading::{Library, Symbol};
use pipe_core::{
    debug,
    modules::{Module, Request, ID},
};
use pipe_parser::Pipe as PipeParse;
use serde_json::Value;
use std::{collections::HashMap, fmt::Debug, sync::Arc};
use std::{
    path::PathBuf,
    str::FromStr,
    sync::mpsc::{Receiver, Sender},
};
use std::{sync::mpsc, thread};

use crate::pipe::{ModuleType, Pipe};
use crate::pipeline::Pipeline;
#[derive(Debug, Clone)]
pub struct ModuleInner {
    pub name: String,
    pub module_type: ModuleType,
}

#[derive(Debug, Clone)]
pub struct Modules {
    pub bins: Bins,
    aliases: Aliases,
}

impl Modules {
    pub(crate) fn get(&self, owner: &str, alias: &str) -> ModuleInner {
        self.aliases.get(owner).unwrap().get(alias).unwrap().clone()
    }

    pub(crate) fn get_bin(&self, key: &str) -> Bin {
        self.bins.get(key).unwrap().clone()
    }
}

type Alias = HashMap<String, ModuleInner>;
type Pipelines = HashMap<String, Pipeline>;
type Aliases = HashMap<String, Alias>;
type Bins = HashMap<String, Bin>;

#[derive(Debug, Clone)]
pub struct Bin {
    pub key: String,
}

impl Bin {
    pub fn extract(&self) -> Box<dyn Module> {
        let lib = match Library::new(self.key.clone()) {
            Ok(lib) => lib,
            Err(err) => panic!("Error: {}; Filename: {}", err, self.key.clone()),
        };
        let bin = unsafe {
            let constructor: Symbol<unsafe extern "C" fn() -> *mut dyn Module> =
                lib.get(b"_Module").unwrap();
            let boxed_raw = constructor();
            Box::from_raw(boxed_raw)
        };

        bin
    }
}

#[derive(Debug)]
pub struct PipelineResponse {
    pub payload: Result<Option<Value>, Option<Value>>,
    pub attach: u32,
    pub origin: u32,
    pub trace_id: u32,
}

#[derive(Debug, Clone)]
pub struct PipelineSender {
    pub tx: Sender<Request>,
    pub id: u32,
}

#[derive(Debug)]
pub struct Runtime {
    pipelines: Pipelines,
    pipelines_keys: Vec<String>,
    modules: Modules,
    main: String,
}

impl Runtime {
    pub fn builder(main_path: &str) -> Result<Self, ()> {
        let target = main_path;
        let mut targets = vec![target.to_string()];
        let mut aliases: Aliases = HashMap::new();
        let mut pipelines: Pipelines = HashMap::new();
        let mut bins: Bins = HashMap::new();
        let mut pipelines_keys = Vec::new();
        let main = PathBuf::from_str(target)
            .unwrap()
            .canonicalize()
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();

        let mut id: ID = 0;

        loop {
            let index = if targets.len() > 0 {
                targets.len() - 1
            } else {
                break;
            };

            id += 1;

            let path = PathBuf::from_str(targets.get(index).unwrap()).unwrap();
            let target = path.canonicalize().unwrap();
            let target_key = target.to_str().unwrap().to_string();

            let pipe = match PipeParse::from_path(&target_key) {
                Ok(value) => Pipe::new(&value),
                Err(_) => return Err(()),
            };

            let path_base = target.parent().unwrap().to_str().unwrap();

            let pipeline = Pipeline::new(id, target_key.clone(), pipe.clone());
            pipelines_keys.push(target_key.clone());
            pipelines.insert(target_key.clone(), pipeline);

            for module in pipe.modules.unwrap().iter() {
                let path_raw = format!("{}/{}", path_base, module.path);
                let module_key = match PathBuf::from_str(&path_raw).unwrap().canonicalize() {
                    Ok(path) => path.to_str().unwrap().to_string(),
                    Err(err) => {
                        panic!("{}: {:?}", path_raw, err);
                    }
                };

                match aliases.get_mut(&target_key) {
                    Some(group) => {
                        group.insert(
                            module.name.clone(),
                            ModuleInner {
                                name: module_key.clone(),
                                module_type: module.module_type.clone(),
                            },
                        );
                    }
                    None => {
                        aliases.insert(target_key.clone(), {
                            let mut group: Alias = HashMap::new();

                            group.insert(
                                module.name.clone(),
                                ModuleInner {
                                    name: module_key.clone(),
                                    module_type: module.module_type.clone(),
                                },
                            );

                            group
                        });
                    }
                }

                if module.module_type.eq(&ModuleType::Bin) {
                    if bins.get(&module_key).is_none() {
                        bins.insert(
                            module_key.clone(),
                            Bin {
                                key: module_key.clone(),
                            },
                        );
                    }
                } else if module.module_type.eq(&ModuleType::Pipeline) {
                    if pipelines.get(&module_key).is_none() {
                        let new_target = format!("{}/{}", path_base, module.path);
                        targets.push(new_target)
                    }
                }
            }

            targets.remove(index);
        }

        Ok(Self {
            pipelines,
            modules: Modules { bins, aliases },
            main,
            pipelines_keys,
        })
    }

    fn _get_main(&self) -> &Pipeline {
        self.pipelines.get(&self.main).unwrap()
    }

    pub fn start(&self) {
        debug!("START");

        let (sender_pipeline, receiver_pipeline): (
            Sender<PipelineSender>,
            Receiver<PipelineSender>,
        ) = mpsc::channel();
        let (sender_control, receiver_control): (
            Sender<PipelineResponse>,
            Receiver<PipelineResponse>,
        ) = mpsc::channel();

        let pipes = self.pipelines.clone();
        let modules = unsafe { self.modules.clone() };
        for key in self.pipelines_keys.iter() {
            let pipeline = pipes.get(key).unwrap().clone();
            let modules = modules.clone();
            let sender_pipeline = sender_pipeline.clone();
            let sender_control = sender_control.clone();

            thread::spawn(move || {
                match pipeline.start(modules.clone(), sender_pipeline, sender_control) {
                    Ok(_) => todo!(),
                    Err(_) => panic!("Pipeline Error: {}", pipeline.key),
                };
            });
        }

        let mut pipeline_senders = HashMap::new();
        let mut pipelines_done = self.pipelines_keys.len() - 1;
        for pipeline_sender in receiver_pipeline {
            pipeline_senders.insert(pipeline_sender.id, pipeline_sender.tx);

            if pipelines_done == 0 {
                break;
            }

            pipelines_done -= 1;
        }

        for pipeline_response in receiver_control {
            let sender = pipeline_senders.get(&pipeline_response.attach).unwrap();
            match sender.send(Request {
                origin: pipeline_response.origin,
                payload: pipeline_response.payload,
                steps: None,
                trace_id: pipeline_response.trace_id,
            }) {
                Ok(_) => continue,
                Err(err) => panic!("{:?}", err),
            }
        }

        debug!("END");

        // println!("done");

        // for sender in self.receiver {
        //     senders.insert(sender.id, sender.tx);
        // }

        // println!("{:?}", senders);

        // for response in self.receiver_control {}
    }
}
