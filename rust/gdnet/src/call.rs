//! Calls (docs/PLAN_NETCODE.md 1 rung 2, 2.1 "Auto-exposed methods", 3.1, WP6): the verbs
//! `Net.sync` / `server` / `owner` / `others` / `peer`, `NetCall` (the awaitable result),
//! `NetCallInfo` (sender info a method may take as its last parameter), and the argument rules.
//!
//! A verb takes a `Callable` on a part of a managed entity (a script method or a built-in one:
//! `$Anim.play`, `$Label.set_text`; bound arguments count, appended after the given ones) and up to
//! eight arguments. The call is checked at the call site (the object is a part of a managed entity, the
//! method is in the part's table and does not begin with `_`, every argument can travel); a failure is
//! an error on the returned `NetCall`, printed once, and nothing is sent. netcore then routes it
//! (netcore/src/call.rs); the manager runs it where the route says (manager.rs, "Calls").
//!
//! Arguments: `null`, bools, ints, floats and Strings travel directly; a `Resource` with a path travels
//! as that path and is `load()`ed on receipt; any other value that holds no object (vectors, colors,
//! StringNames, packed arrays, arrays and dictionaries of such values) travels as `var_to_bytes`.
//! Objects (a Resource without a path too), Callables, Signals and RIDs are refused.
//!
//! Inside a replay (`Net.is_fresh()` false) every verb does nothing and returns a `NetCall` already
//! resolved with `ok` and `suppressed` (calls fire in forward ticks only).
//!
//! `Net.server(...)` returns the `completed` signal of its `NetCall`, so `await Net.server(f, x)` is
//! the server's return value (`null` on an error; the signal's object is the `NetCall`). The other verbs
//! return the `NetCall` itself (`ok`, `error` once `completed`). `completed` is emitted deferred, so it
//! is never missed by an `await` that follows the call.

use crate::manager::entity_root_of;
use godot::classes::{Node, Resource, ResourceLoader};
use godot::global::{bytes_to_var, var_to_bytes};
use godot::prelude::*;
use netcore::call::{decode_args, decode_value, encode_args, encode_value, Arg, Verb};

/// The value of an argument slot the caller left out (an RID: RIDs never travel, so it is never a real
/// argument).
pub const NO_ARG: u64 = 0x6764_6e65_745f_6e6f;

/// One optional argument slot of a verb: any Variant (Godot sees the parameter as `Variant`). gdext
/// 0.5.5 has no varargs `#[func]` and refuses `Variant` defaults, so the verbs take eight of these
/// with the default [`NetArg::absent`].
#[derive(Clone, Debug)]
pub struct NetArg(pub Variant);

impl GodotConvert for NetArg {
    type Via = Variant;

    fn godot_shape() -> godot::meta::shape::GodotShape {
        <Variant as GodotConvert>::godot_shape()
    }
}

impl ToGodot for NetArg {
    type Pass = godot::meta::conv::ByValue;

    fn to_godot(&self) -> Variant {
        self.0.clone()
    }
}

impl FromGodot for NetArg {
    fn try_from_godot(via: Variant) -> Result<Self, godot::meta::error::ConvertError> {
        Ok(NetArg(via))
    }
}

// SAFETY: the only default value of a `NetArg` parameter is `NetArg::absent()`, a Variant holding an RID
// (a plain 64-bit value without a reference count), created per call; cloning it is thread-safe and it
// offers no way to mutate a shared value.
unsafe impl godot::meta::GodotImmutable for NetArg {}

impl NetArg {
    /// The default of a left-out slot.
    pub fn absent() -> Self {
        NetArg(Rid::new(NO_ARG).to_variant())
    }

    /// The value given, or nil for a left-out slot.
    pub fn or_nil(self) -> Variant {
        if self.is_absent() {
            Variant::nil()
        } else {
            self.0
        }
    }

    fn is_absent(&self) -> bool {
        self.0.try_to::<Rid>().is_ok_and(|r| r == Rid::new(NO_ARG))
    }
}

/// The given arguments, up to the first left-out slot.
pub fn collect_args(slots: [NetArg; 8]) -> Vec<Variant> {
    slots
        .into_iter()
        .take_while(|a| !a.is_absent())
        .map(|a| a.0)
        .collect()
}

/// The awaitable result of a call.
#[derive(GodotClass)]
#[class(init, base = RefCounted)]
pub struct NetCall {
    base: Base<RefCounted>,
    /// True once the call resolved without an error (sent and routed, or run, or answered).
    #[var(get = get_ok, no_set)]
    ok: bool,
    /// Why the call failed ("" when it did not).
    #[var(get = get_error, no_set)]
    error: GString,
    /// The return value (`Net.server`: the server's; a call run here: the method's), else null.
    #[var(get = get_result, no_set)]
    result: Variant,
    /// The call was made inside a replay and did nothing.
    #[var(get = get_suppressed, no_set)]
    suppressed: bool,
    done: bool,
}

#[godot_api]
impl NetCall {
    /// Emitted once when the call resolves, with the return value (null on an error).
    #[signal]
    fn completed(value: Variant);

    #[func]
    fn get_ok(&self) -> bool {
        self.ok
    }

    #[func]
    fn get_error(&self) -> GString {
        self.error.clone()
    }

    #[func]
    fn get_result(&self) -> Variant {
        self.result.clone()
    }

    #[func]
    fn get_suppressed(&self) -> bool {
        self.suppressed
    }

    /// True once the call resolved (`completed` follows, deferred).
    #[func]
    pub fn is_done(&self) -> bool {
        self.done
    }
}

impl NetCall {
    pub fn fresh() -> Gd<NetCall> {
        NetCall::new_gd()
    }

    /// Resolves the call (once) and schedules `completed`.
    pub fn resolve(call: &Gd<NetCall>, result: Result<Variant, String>) {
        let mut c = call.clone();
        {
            let mut b = c.bind_mut();
            if b.done {
                return;
            }
            b.done = true;
            match result {
                Ok(v) => {
                    b.ok = true;
                    b.result = v;
                }
                Err(e) => {
                    b.ok = false;
                    b.error = GString::from(e.as_str());
                    b.result = Variant::nil();
                }
            }
        }
        // Emitted from a deferred Rust callable that owns a reference, so the call lives until then
        // (an `await` on the signal holds none) and is released outside any bind.
        let held = c.clone();
        let emit = Callable::from_fn("NetCall.completed", move |_| {
            let v = held.bind().result.clone();
            let mut obj = held.clone().upcast::<RefCounted>();
            obj.emit_signal("completed", &[v]);
            Variant::nil()
        });
        emit.to_variant().call("call_deferred", &[]);
    }

    /// A call that did nothing because it was made inside a replay.
    pub fn suppressed() -> Gd<NetCall> {
        let call = NetCall::fresh();
        call.clone().bind_mut().suppressed = true;
        NetCall::resolve(&call, Ok(Variant::nil()));
        call
    }

    /// A call that failed at the call site: the error is printed and nothing is sent.
    pub fn failed(reason: String) -> Gd<NetCall> {
        godot_error!("gdnet: {reason}");
        let call = NetCall::fresh();
        NetCall::resolve(&call, Err(reason));
        call
    }
}

/// What a method that declares a trailing `NetCallInfo` parameter receives.
#[derive(GodotClass)]
#[class(init, base = RefCounted)]
pub struct NetCallInfo {
    base: Base<RefCounted>,
    /// The peer that made the call (1 = the server).
    #[var]
    sender: i64,
    /// The sender's tick when it made the call.
    #[var]
    tick: i64,
    /// True when the call was made on this peer.
    #[var]
    is_local: bool,
}

impl NetCallInfo {
    pub fn make(sender: u32, tick: u64, is_local: bool) -> Gd<NetCallInfo> {
        let mut i = NetCallInfo::new_gd();
        {
            let mut b = i.bind_mut();
            b.sender = i64::from(sender);
            b.tick = tick as i64;
            b.is_local = is_local;
        }
        i
    }
}

/// A Variant as it travels, or why it cannot.
pub fn to_arg(v: &Variant) -> Result<Arg, String> {
    Ok(match v.get_type() {
        VariantType::NIL => Arg::Nil,
        VariantType::BOOL => Arg::Bool(v.to()),
        VariantType::INT => Arg::Int(v.to()),
        VariantType::FLOAT => Arg::Float(v.to()),
        VariantType::STRING => Arg::Str(v.to::<GString>().to_string()),
        VariantType::OBJECT => {
            let Ok(obj) = v.try_to::<Gd<Object>>() else {
                return Ok(Arg::Nil); // a null object
            };
            match obj.clone().try_cast::<Resource>() {
                Ok(r) if !r.get_path().is_empty() && !r.get_path().to_string().contains("::") => {
                    Arg::Resource(r.get_path().to_string())
                }
                Ok(_) => return Err(String::from(
                    "a Resource without a resource path cannot be sent (save it to a file so it travels by path, or pass its data)",
                )),
                Err(_) => {
                    return Err(std::format!(
                        "an Object ({}) cannot be sent as an argument; pass its data, or the net id of an entity (Net.net_id)",
                        obj.get_class()
                    ))
                }
            }
        }
        VariantType::ARRAY | VariantType::DICTIONARY => {
            check_nested(v)?;
            Arg::Var(var_to_bytes(v).to_vec())
        }
        other => {
            check_nested(v)?;
            let _ = other;
            Arg::Var(var_to_bytes(v).to_vec())
        }
    })
}

/// Refuses values that hold an object, a Callable, a Signal or an RID (at any depth).
fn check_nested(v: &Variant) -> Result<(), String> {
    match v.get_type() {
        VariantType::OBJECT if !v.is_nil() => Err(String::from(
            "an Object inside an Array or Dictionary cannot be sent; pass its data",
        )),
        VariantType::CALLABLE => Err(String::from(
            "a Callable cannot be sent as an argument; call the method on the receiving side instead",
        )),
        VariantType::SIGNAL => Err(String::from("a Signal cannot be sent as an argument")),
        VariantType::RID => Err(String::from(
            "an RID cannot be sent as an argument: RIDs are local to each peer",
        )),
        VariantType::ARRAY => {
            let a: VarArray = v.to();
            for x in a.iter_shared() {
                check_nested(&x)?;
            }
            Ok(())
        }
        VariantType::DICTIONARY => {
            let d: VarDictionary = v.to();
            for (k, x) in d.iter_shared() {
                check_nested(&k)?;
                check_nested(&x)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Every argument as it travels, encoded; the first refusal names the argument.
pub fn encode(args: &[Variant]) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(args.len());
    for (i, a) in args.iter().enumerate() {
        match to_arg(a) {
            Ok(x) => out.push(x),
            Err(e) => return Err(std::format!("argument {}: {e}", i + 1)),
        }
    }
    Ok(encode_args(&out))
}

/// A received argument as a Variant (a resource path is loaded).
pub fn from_arg(a: Arg) -> Variant {
    match a {
        Arg::Nil => Variant::nil(),
        Arg::Bool(b) => b.to_variant(),
        Arg::Int(i) => i.to_variant(),
        Arg::Float(f) => f.to_variant(),
        Arg::Str(s) => GString::from(s.as_str()).to_variant(),
        Arg::Resource(p) => ResourceLoader::singleton()
            .load(p.as_str())
            .map_or_else(Variant::nil, |r| r.to_variant()),
        Arg::Var(b) => bytes_to_var(&PackedByteArray::from(b.as_slice())),
    }
}

/// Received argument bytes as Variants (None when malformed).
pub fn decode(bytes: &[u8]) -> Option<Vec<Variant>> {
    decode_args(bytes).map(|v| v.into_iter().map(from_arg).collect())
}

/// A return value as it travels (an unsendable value travels as null, with a warning).
pub fn encode_return(v: &Variant) -> Vec<u8> {
    match to_arg(v) {
        Ok(a) => encode_value(&a),
        Err(e) => {
            godot_warn!("gdnet: a call's return value cannot be sent ({e}); the caller gets null");
            encode_value(&Arg::Nil)
        }
    }
}

/// A received return value.
pub fn decode_return(bytes: &[u8]) -> Variant {
    decode_value(bytes).map_or_else(Variant::nil, from_arg)
}

/// The bound arguments of a callable (`f.bind(7)`), in order.
fn bound_args(callable: &Callable) -> Vec<Variant> {
    if callable.get_bound_arguments_count() == 0 {
        return Vec::new();
    }
    callable
        .to_variant()
        .call("get_bound_arguments", &[])
        .try_to::<VarArray>()
        .map(|a| a.iter_shared().collect())
        .unwrap_or_default()
}

/// The verbs' common path: checks the call at the call site and hands it to the entity's manager.
pub fn make_call(
    verb: Verb,
    target: u32,
    callable: &Callable,
    given: Vec<Variant>,
    unreliable: bool,
) -> Gd<NetCall> {
    if !crate::tick::is_fresh() {
        return NetCall::suppressed();
    }
    let verb_name = verb_name(verb);
    let bound = bound_args(callable);
    if callable.is_custom() && bound.is_empty() {
        return NetCall::failed(std::format!(
            "Net.{verb_name}: the Callable is a lambda or a custom callable; pass a method of a managed node (e.g. open_door or $Anim.play)"
        ));
    }
    let Some(obj) = callable.object() else {
        return NetCall::failed(std::format!(
            "Net.{verb_name}: the Callable has no object; pass a method of a managed node"
        ));
    };
    let Some(method) = callable.method_name() else {
        return NetCall::failed(std::format!(
            "Net.{verb_name}: the Callable names no method"
        ));
    };
    let method_s = method.to_string();
    let Ok(node) = obj.try_cast::<Node>() else {
        return NetCall::failed(std::format!(
            "Net.{verb_name}({method_s}): the Callable's object is not a Node, so it is not part of a managed entity"
        ));
    };
    if method_s.starts_with('_') {
        return NetCall::failed(std::format!(
            "Net.{verb_name}: {method_s} cannot be called over the network: methods beginning with _ are never network-callable"
        ));
    }
    let mut args = given;
    args.extend(bound);
    let Some((root, mut manager)) = entity_root_of(&node) else {
        // Not managed. Without a session in its branch it is single player: it just runs.
        if crate::manager::session_of(&node).is_some_and(|s| s.bind().is_connected()) {
            return NetCall::failed(std::format!(
                "Net.{verb_name}({method_s}): {} is not part of a managed entity (put it under a NetManager, or Net.manage() its parent)",
                node.get_path()
            ));
        }
        let call = NetCall::fresh();
        let info = NetCallInfo::make(1, 0, true);
        let v = crate::manager::invoke(
            &node,
            &method,
            &args,
            wants_info(&node, &method_s).then_some(info),
        );
        NetCall::resolve(&call, Ok(v));
        return call;
    };
    let bytes = match encode(&args) {
        Ok(b) => b,
        Err(e) => {
            return NetCall::failed(std::format!("Net.{verb_name}({method_s}): {e}"));
        }
    };
    let call = NetCall::fresh();
    let req = crate::manager::CallReq {
        verb,
        target,
        method: method.clone(),
        args,
        bytes,
        unreliable,
        call: call.clone(),
    };
    if let Err(e) = manager.bind_mut().call_out(&root, &node, req) {
        return NetCall::failed(std::format!("Net.{verb_name}({method_s}): {e}"));
    }
    call
}

/// Whether `method` of an unmanaged `node` declares a trailing `NetCallInfo`.
fn wants_info(node: &Gd<Node>, method: &str) -> bool {
    crate::methods::part_methods(node)
        .by_hash
        .get(&netcore::call::method_hash(method))
        .is_some_and(|m| m.wants_info)
}

pub fn verb_name(v: Verb) -> &'static str {
    match v {
        Verb::Sync => "sync",
        Verb::Server => "server",
        Verb::Owner => "owner",
        Verb::Others => "others",
        Verb::Peer => "peer",
    }
}
