use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use pirs_ai::{CompletionOptions, Context, LlmProvider, Message};
use tokio_util::sync::CancellationToken;

use crate::agent_loop::{run_agent_loop, CascadeConfig, LoopConfig, VisibleTools};
use crate::compaction::{compact_messages, CompactionConfig};
use crate::events::{AgentEvent, Emit, Hooks};
use crate::tool::{AgentTool, ExecutionMode};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QueueMode {
    All,
    #[default]
    OneAtATime,
}

#[derive(thiserror::Error, Debug)]
pub enum AgentError {
    #[error("agent is already running; use steer() or follow_up() instead")]
    AlreadyRunning,
    #[error("cannot continue: last message is from the assistant")]
    NothingToContinue,
}

pub struct Agent {
    provider: Arc<dyn LlmProvider>,
    pub system_prompt: String,
    pub model: String,
    pub tools: Vec<Arc<dyn AgentTool>>,
    pub messages: Vec<Message>,
    completion: CompletionOptions,
    tool_execution: ExecutionMode,
    pub compaction: Option<CompactionConfig>,
    visible_tools: Option<VisibleTools>,
    cascade: Option<CascadeConfig>,
    extra_usage: Arc<Mutex<pirs_ai::Usage>>,
    hooks: Hooks,
    listeners: Vec<Emit>,
    steering: Arc<Mutex<VecDeque<Message>>>,
    follow_up: Arc<Mutex<VecDeque<Message>>>,
    steering_mode: QueueMode,
    follow_up_mode: QueueMode,
    running: Arc<AtomicBool>,
    cancel: CancellationToken,
}

impl Agent {
    pub fn new(provider: Arc<dyn LlmProvider>, model: impl Into<String>) -> Self {
        Agent {
            provider,
            system_prompt: "You are a helpful assistant.".to_string(),
            model: model.into(),
            tools: Vec::new(),
            messages: Vec::new(),
            completion: CompletionOptions::default(),
            tool_execution: ExecutionMode::Parallel,
            compaction: Some(CompactionConfig::default()),
            visible_tools: None,
            cascade: None,
            extra_usage: Arc::new(Mutex::new(pirs_ai::Usage::default())),
            hooks: Hooks::default(),
            listeners: Vec::new(),
            steering: Arc::new(Mutex::new(VecDeque::new())),
            follow_up: Arc::new(Mutex::new(VecDeque::new())),
            steering_mode: QueueMode::default(),
            follow_up_mode: QueueMode::default(),
            running: Arc::new(AtomicBool::new(false)),
            cancel: CancellationToken::new(),
        }
    }

    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = prompt.into();
        self
    }

    pub fn with_tools(mut self, tools: Vec<Arc<dyn AgentTool>>) -> Self {
        self.tools = tools;
        self
    }

    pub fn with_completion(mut self, completion: CompletionOptions) -> Self {
        self.completion = completion;
        self
    }

    pub fn with_tool_execution(mut self, mode: ExecutionMode) -> Self {
        self.tool_execution = mode;
        self
    }

    pub fn with_compaction(mut self, config: Option<CompactionConfig>) -> Self {
        self.compaction = config;
        self
    }

    pub fn with_visible_tools(mut self, visible: Option<VisibleTools>) -> Self {
        self.visible_tools = visible;
        self
    }

    pub fn with_cascade(mut self, cascade: Option<CascadeConfig>) -> Self {
        self.cascade = cascade;
        self
    }

    pub fn compaction_enabled(&self) -> bool {
        self.compaction.is_some()
    }

    pub fn usage_report(&self) -> crate::usage::UsageReport {
        let extra = self.extra_usage.lock().unwrap().clone();
        crate::usage::usage_report(&self.messages, extra)
    }

    pub fn extra_usage_handle(&self) -> Arc<Mutex<pirs_ai::Usage>> {
        Arc::clone(&self.extra_usage)
    }

    pub fn set_extra_usage_handle(&mut self, handle: Arc<Mutex<pirs_ai::Usage>>) {
        self.extra_usage = handle;
    }

    pub async fn compact_now(&mut self) -> bool {
        let Some(cfg) = self.compaction.clone() else {
            return false;
        };
        let listeners = self.listeners.clone();
        let emit: Emit = Arc::new(move |event: AgentEvent| {
            for l in &listeners {
                l(event.clone());
            }
        });
        let provider = Arc::clone(&self.provider);
        compact_messages(
            &provider,
            &self.model,
            &mut self.messages,
            &cfg,
            &emit,
            self.cancel.clone(),
            &self.extra_usage,
        )
        .await
    }

    pub fn with_hooks(mut self, hooks: Hooks) -> Self {
        self.hooks = hooks;
        self
    }

    pub fn with_queue_modes(mut self, steering: QueueMode, follow_up: QueueMode) -> Self {
        self.steering_mode = steering;
        self.follow_up_mode = follow_up;
        self
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    pub fn subscribe(&mut self, listener: Emit) {
        self.listeners.push(listener);
    }

    pub fn steer(&self, message: Message) {
        self.steering.lock().unwrap().push_back(message);
    }

    pub fn follow_up(&self, message: Message) {
        self.follow_up.lock().unwrap().push_back(message);
    }

    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    pub fn cancel_handle(&self) -> CancellationToken {
        self.cancel.clone()
    }

    pub fn steer_sender(&self) -> impl Fn(Message) + Send + 'static {
        let queue = Arc::clone(&self.steering);
        move |msg: Message| {
            queue.lock().unwrap().push_back(msg);
        }
    }

    pub async fn prompt(&mut self, text: impl Into<String>) -> Result<Vec<Message>, AgentError> {
        self.run(Some(Message::user(text))).await
    }

    pub async fn prompt_messages(
        &mut self,
        prompts: Vec<Message>,
    ) -> Result<Vec<Message>, AgentError> {
        self.run_many(prompts).await
    }

    pub async fn continue_(&mut self) -> Result<Vec<Message>, AgentError> {
        if self.messages.last().map(|m| m.is_assistant()).unwrap_or(false) {
            return Err(AgentError::NothingToContinue);
        }
        self.run(None).await
    }

    async fn run(&mut self, prompt: Option<Message>) -> Result<Vec<Message>, AgentError> {
        match prompt {
            Some(p) => self.run_many(vec![p]).await,
            None => self.run_many(vec![]).await,
        }
    }

    async fn run_many(&mut self, prompts: Vec<Message>) -> Result<Vec<Message>, AgentError> {
        let run = self.begin_prompt(prompts)?;
        let (full_messages, new_messages) = run.await;
        self.complete_run(full_messages);
        Ok(new_messages)
    }

    /// Starts a run, returning an owned future resolving to
    /// `(full_messages, new_messages)`. The caller must pass the full list to
    /// `complete_run` (even on abort/error). While the future is alive the
    /// agent stays usable for `steer`/`follow_up`/`cancel`.
    pub fn begin_prompt(
        &mut self,
        prompts: Vec<Message>,
    ) -> Result<impl std::future::Future<Output = (Vec<Message>, Vec<Message>)> + Send + 'static, AgentError>
    {
        if self.running.swap(true, Ordering::SeqCst) {
            return Err(AgentError::AlreadyRunning);
        }
        self.cancel = CancellationToken::new();

        let steering = Arc::clone(&self.steering);
        let steering_mode = self.steering_mode;
        let follow_up = Arc::clone(&self.follow_up);
        let follow_up_mode = self.follow_up_mode;

        let mut hooks = self.hooks.clone();
        let ext_steering = hooks.get_steering_messages.take();
        hooks.get_steering_messages = Some(Arc::new(move || {
            let mut msgs = drain_queue(&steering, steering_mode);
            if let Some(f) = &ext_steering {
                msgs.extend(f());
            }
            msgs
        }));
        let ext_follow_up = hooks.get_follow_up_messages.take();
        hooks.get_follow_up_messages = Some(Arc::new(move || {
            let mut msgs = drain_queue(&follow_up, follow_up_mode);
            if let Some(f) = &ext_follow_up {
                msgs.extend(f());
            }
            msgs
        }));

        let listeners = self.listeners.clone();
        let emit: Emit = Arc::new(move |event: AgentEvent| {
            for l in &listeners {
                l(event.clone());
            }
        });

        let mut context = Context {
            system_prompt: Some(self.system_prompt.clone()),
            messages: std::mem::take(&mut self.messages),
            tools: vec![],
        };

        let config = LoopConfig {
            model: self.model.clone(),
            completion: self.completion.clone(),
            tool_execution: self.tool_execution,
            hooks,
            compaction: self.compaction.clone(),
            visible_tools: self.visible_tools.clone(),
            extra_usage: Arc::clone(&self.extra_usage),
            cascade: self.cascade.clone(),
        };

        let tools = self.tools.clone();
        let provider = Arc::clone(&self.provider);
        let cancel = self.cancel.clone();

        Ok(async move {
            let new_messages =
                run_agent_loop(prompts, &mut context, &tools, &provider, &config, &emit, cancel)
                    .await;
            (context.messages, new_messages)
        })
    }

    pub fn complete_run(&mut self, full_messages: Vec<Message>) {
        self.messages = full_messages;
        self.running.store(false, Ordering::SeqCst);
    }
}

fn drain_queue(queue: &Arc<Mutex<VecDeque<Message>>>, mode: QueueMode) -> Vec<Message> {
    let mut q = queue.lock().unwrap();
    match mode {
        QueueMode::All => q.drain(..).collect(),
        QueueMode::OneAtATime => q.pop_front().into_iter().collect(),
    }
}
