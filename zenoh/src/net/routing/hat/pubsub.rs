//
// Copyright (c) 2023 ZettaScale Technology
//
// This program and the accompanying materials are made available under the
// terms of the Eclipse Public License 2.0 which is available at
// http://www.eclipse.org/legal/epl-2.0, or the Apache License, Version 2.0
// which is available at https://www.apache.org/licenses/LICENSE-2.0.
//
// SPDX-License-Identifier: EPL-2.0 OR Apache-2.0
//
// Contributors:
//   ZettaScale Zenoh Team, <zenoh@zettascale.tech>
//
use super::super::dispatcher::face::FaceState;
use super::super::dispatcher::pubsub::*;
use super::super::dispatcher::resource::{Resource, RoutingContext, SessionContext};
use super::super::dispatcher::tables::{Tables, TablesLock};
use super::super::PREFIX_LIVELINESS;
use super::network::Network;
use super::HatTables;
use petgraph::graph::NodeIndex;
use std::collections::HashMap;
use std::sync::{Arc, RwLockReadGuard};
use zenoh_core::zread;
use zenoh_protocol::{
    core::{key_expr::keyexpr, Reliability, WhatAmI, WireExpr, ZenohId},
    network::declare::{
        common::ext::WireExprType, ext, subscriber::ext::SubscriberInfo, Declare, DeclareBody,
        DeclareSubscriber, Mode, UndeclareSubscriber,
    },
};
use zenoh_sync::get_mut_unchecked;

#[inline]
fn send_sourced_subscription_to_net_childs(
    tables: &Tables,
    net: &Network,
    childs: &[NodeIndex],
    res: &Arc<Resource>,
    src_face: Option<&Arc<FaceState>>,
    sub_info: &SubscriberInfo,
    routing_context: Option<RoutingContext>,
) {
    for child in childs {
        if net.graph.contains_node(*child) {
            match tables.get_face(&net.graph[*child].zid).cloned() {
                Some(mut someface) => {
                    if src_face.is_none() || someface.id != src_face.unwrap().id {
                        let key_expr = Resource::decl_key(res, &mut someface);

                        log::debug!("Send subscription {} on {}", res.expr(), someface);

                        someface.primitives.send_declare(Declare {
                            ext_qos: ext::QoSType::declare_default(),
                            ext_tstamp: None,
                            ext_nodeid: ext::NodeIdType {
                                node_id: routing_context.unwrap_or(0),
                            },
                            body: DeclareBody::DeclareSubscriber(DeclareSubscriber {
                                id: 0, // TODO
                                wire_expr: key_expr,
                                ext_info: *sub_info,
                            }),
                        });
                    }
                }
                None => log::trace!("Unable to find face for zid {}", net.graph[*child].zid),
            }
        }
    }
}

#[inline]
fn propagate_simple_subscription_to(
    tables: &mut Tables,
    dst_face: &mut Arc<FaceState>,
    res: &Arc<Resource>,
    sub_info: &SubscriberInfo,
    src_face: &mut Arc<FaceState>,
    full_peer_net: bool,
) {
    if (src_face.id != dst_face.id || res.expr().starts_with(PREFIX_LIVELINESS))
        && !dst_face.local_subs.contains(res)
        && match tables.whatami {
            WhatAmI::Router => {
                if full_peer_net {
                    dst_face.whatami == WhatAmI::Client
                } else {
                    dst_face.whatami != WhatAmI::Router
                        && (src_face.whatami != WhatAmI::Peer
                            || dst_face.whatami != WhatAmI::Peer
                            || tables.hat.failover_brokering(src_face.zid, dst_face.zid))
                }
            }
            WhatAmI::Peer => {
                if full_peer_net {
                    dst_face.whatami == WhatAmI::Client
                } else {
                    src_face.whatami == WhatAmI::Client || dst_face.whatami == WhatAmI::Client
                }
            }
            _ => src_face.whatami == WhatAmI::Client || dst_face.whatami == WhatAmI::Client,
        }
    {
        get_mut_unchecked(dst_face).local_subs.insert(res.clone());
        let key_expr = Resource::decl_key(res, dst_face);
        dst_face.primitives.send_declare(Declare {
            ext_qos: ext::QoSType::declare_default(),
            ext_tstamp: None,
            ext_nodeid: ext::NodeIdType::default(),
            body: DeclareBody::DeclareSubscriber(DeclareSubscriber {
                id: 0, // TODO
                wire_expr: key_expr,
                ext_info: *sub_info,
            }),
        });
    }
}

fn propagate_simple_subscription(
    tables: &mut Tables,
    res: &Arc<Resource>,
    sub_info: &SubscriberInfo,
    src_face: &mut Arc<FaceState>,
) {
    let full_peer_net = tables.hat.full_net(WhatAmI::Peer);
    for mut dst_face in tables
        .faces
        .values()
        .cloned()
        .collect::<Vec<Arc<FaceState>>>()
    {
        propagate_simple_subscription_to(
            tables,
            &mut dst_face,
            res,
            sub_info,
            src_face,
            full_peer_net,
        );
    }
}

fn propagate_sourced_subscription(
    tables: &Tables,
    res: &Arc<Resource>,
    sub_info: &SubscriberInfo,
    src_face: Option<&Arc<FaceState>>,
    source: &ZenohId,
    net_type: WhatAmI,
) {
    let net = tables.hat.get_net(net_type).unwrap();
    match net.get_idx(source) {
        Some(tree_sid) => {
            if net.trees.len() > tree_sid.index() {
                send_sourced_subscription_to_net_childs(
                    tables,
                    net,
                    &net.trees[tree_sid.index()].childs,
                    res,
                    src_face,
                    sub_info,
                    Some(tree_sid.index() as u16),
                );
            } else {
                log::trace!(
                    "Propagating sub {}: tree for node {} sid:{} not yet ready",
                    res.expr(),
                    tree_sid.index(),
                    source
                );
            }
        }
        None => log::error!(
            "Error propagating sub {}: cannot get index of {}!",
            res.expr(),
            source
        ),
    }
}

fn register_router_subscription(
    tables: &mut Tables,
    face: &mut Arc<FaceState>,
    res: &mut Arc<Resource>,
    sub_info: &SubscriberInfo,
    router: ZenohId,
) {
    if !res.context().router_subs.contains(&router) {
        // Register router subscription
        {
            log::debug!(
                "Register router subscription {} (router: {})",
                res.expr(),
                router
            );
            get_mut_unchecked(res)
                .context_mut()
                .router_subs
                .insert(router);
            tables.hat.router_subs.insert(res.clone());
        }

        // Propagate subscription to routers
        propagate_sourced_subscription(tables, res, sub_info, Some(face), &router, WhatAmI::Router);
    }
    // Propagate subscription to peers
    if tables.hat.full_net(WhatAmI::Peer) && face.whatami != WhatAmI::Peer {
        register_peer_subscription(tables, face, res, sub_info, tables.zid)
    }

    // Propagate subscription to clients
    propagate_simple_subscription(tables, res, sub_info, face);
}

pub fn declare_router_subscription(
    tables: &TablesLock,
    rtables: RwLockReadGuard<Tables>,
    face: &mut Arc<FaceState>,
    expr: &WireExpr,
    sub_info: &SubscriberInfo,
    router: ZenohId,
) {
    match rtables
        .get_mapping(face, &expr.scope, expr.mapping)
        .cloned()
    {
        Some(mut prefix) => {
            let res = Resource::get_resource(&prefix, &expr.suffix);
            let (mut res, mut wtables) =
                if res.as_ref().map(|r| r.context.is_some()).unwrap_or(false) {
                    drop(rtables);
                    let wtables = zwrite!(tables.tables);
                    (res.unwrap(), wtables)
                } else {
                    let mut fullexpr = prefix.expr();
                    fullexpr.push_str(expr.suffix.as_ref());
                    let mut matches = keyexpr::new(fullexpr.as_str())
                        .map(|ke| Resource::get_matches(&rtables, ke))
                        .unwrap_or_default();
                    drop(rtables);
                    let mut wtables = zwrite!(tables.tables);
                    let mut res =
                        Resource::make_resource(&mut wtables, &mut prefix, expr.suffix.as_ref());
                    matches.push(Arc::downgrade(&res));
                    Resource::match_resource(&wtables, &mut res, matches);
                    (res, wtables)
                };
            register_router_subscription(&mut wtables, face, &mut res, sub_info, router);
            disable_matches_data_routes(&mut wtables, &mut res);
            drop(wtables);

            let rtables = zread!(tables.tables);
            let matches_data_routes = compute_matches_data_routes_(&rtables, &res);
            drop(rtables);

            let wtables = zwrite!(tables.tables);
            for (mut res, data_routes) in matches_data_routes {
                get_mut_unchecked(&mut res)
                    .context_mut()
                    .update_data_routes(data_routes);
            }
            drop(wtables);
        }
        None => log::error!(
            "Declare router subscription for unknown scope {}!",
            expr.scope
        ),
    }
}

fn register_peer_subscription(
    tables: &mut Tables,
    face: &mut Arc<FaceState>,
    res: &mut Arc<Resource>,
    sub_info: &SubscriberInfo,
    peer: ZenohId,
) {
    if !res.context().peer_subs.contains(&peer) {
        // Register peer subscription
        {
            log::debug!("Register peer subscription {} (peer: {})", res.expr(), peer);
            get_mut_unchecked(res).context_mut().peer_subs.insert(peer);
            tables.hat.peer_subs.insert(res.clone());
        }

        // Propagate subscription to peers
        propagate_sourced_subscription(tables, res, sub_info, Some(face), &peer, WhatAmI::Peer);
    }

    if tables.whatami == WhatAmI::Peer {
        // Propagate subscription to clients
        propagate_simple_subscription(tables, res, sub_info, face);
    }
}

pub fn declare_peer_subscription(
    tables: &TablesLock,
    rtables: RwLockReadGuard<Tables>,
    face: &mut Arc<FaceState>,
    expr: &WireExpr,
    sub_info: &SubscriberInfo,
    peer: ZenohId,
) {
    match rtables
        .get_mapping(face, &expr.scope, expr.mapping)
        .cloned()
    {
        Some(mut prefix) => {
            let res = Resource::get_resource(&prefix, &expr.suffix);
            let (mut res, mut wtables) =
                if res.as_ref().map(|r| r.context.is_some()).unwrap_or(false) {
                    drop(rtables);
                    let wtables = zwrite!(tables.tables);
                    (res.unwrap(), wtables)
                } else {
                    let mut fullexpr = prefix.expr();
                    fullexpr.push_str(expr.suffix.as_ref());
                    let mut matches = keyexpr::new(fullexpr.as_str())
                        .map(|ke| Resource::get_matches(&rtables, ke))
                        .unwrap_or_default();
                    drop(rtables);
                    let mut wtables = zwrite!(tables.tables);
                    let mut res =
                        Resource::make_resource(&mut wtables, &mut prefix, expr.suffix.as_ref());
                    matches.push(Arc::downgrade(&res));
                    Resource::match_resource(&wtables, &mut res, matches);
                    (res, wtables)
                };
            register_peer_subscription(&mut wtables, face, &mut res, sub_info, peer);
            if wtables.whatami == WhatAmI::Router {
                let mut propa_sub_info = *sub_info;
                propa_sub_info.mode = Mode::Push;
                let zid = wtables.zid;
                register_router_subscription(&mut wtables, face, &mut res, &propa_sub_info, zid);
            }
            disable_matches_data_routes(&mut wtables, &mut res);
            drop(wtables);

            let rtables = zread!(tables.tables);
            let matches_data_routes = compute_matches_data_routes_(&rtables, &res);
            drop(rtables);

            let wtables = zwrite!(tables.tables);
            for (mut res, data_routes) in matches_data_routes {
                get_mut_unchecked(&mut res)
                    .context_mut()
                    .update_data_routes(data_routes);
            }
            drop(wtables);
        }
        None => log::error!(
            "Declare router subscription for unknown scope {}!",
            expr.scope
        ),
    }
}

fn register_client_subscription(
    _tables: &mut Tables,
    face: &mut Arc<FaceState>,
    res: &mut Arc<Resource>,
    sub_info: &SubscriberInfo,
) {
    // Register subscription
    {
        let res = get_mut_unchecked(res);
        log::debug!("Register subscription {} for {}", res.expr(), face);
        match res.session_ctxs.get_mut(&face.id) {
            Some(ctx) => match &ctx.subs {
                Some(info) => {
                    if Mode::Pull == info.mode {
                        get_mut_unchecked(ctx).subs = Some(*sub_info);
                    }
                }
                None => {
                    get_mut_unchecked(ctx).subs = Some(*sub_info);
                }
            },
            None => {
                res.session_ctxs.insert(
                    face.id,
                    Arc::new(SessionContext {
                        face: face.clone(),
                        local_expr_id: None,
                        remote_expr_id: None,
                        subs: Some(*sub_info),
                        qabl: None,
                        last_values: HashMap::new(),
                    }),
                );
            }
        }
    }
    get_mut_unchecked(face).remote_subs.insert(res.clone());
}

pub fn declare_client_subscription(
    tables: &TablesLock,
    rtables: RwLockReadGuard<Tables>,
    face: &mut Arc<FaceState>,
    expr: &WireExpr,
    sub_info: &SubscriberInfo,
) {
    log::debug!("Register client subscription");
    match rtables
        .get_mapping(face, &expr.scope, expr.mapping)
        .cloned()
    {
        Some(mut prefix) => {
            let res = Resource::get_resource(&prefix, &expr.suffix);
            let (mut res, mut wtables) =
                if res.as_ref().map(|r| r.context.is_some()).unwrap_or(false) {
                    drop(rtables);
                    let wtables = zwrite!(tables.tables);
                    (res.unwrap(), wtables)
                } else {
                    let mut fullexpr = prefix.expr();
                    fullexpr.push_str(expr.suffix.as_ref());
                    let mut matches = keyexpr::new(fullexpr.as_str())
                        .map(|ke| Resource::get_matches(&rtables, ke))
                        .unwrap_or_default();
                    drop(rtables);
                    let mut wtables = zwrite!(tables.tables);
                    let mut res =
                        Resource::make_resource(&mut wtables, &mut prefix, expr.suffix.as_ref());
                    matches.push(Arc::downgrade(&res));
                    Resource::match_resource(&wtables, &mut res, matches);
                    (res, wtables)
                };

            register_client_subscription(&mut wtables, face, &mut res, sub_info);
            let mut propa_sub_info = *sub_info;
            propa_sub_info.mode = Mode::Push;
            match wtables.whatami {
                WhatAmI::Router => {
                    let zid = wtables.zid;
                    register_router_subscription(
                        &mut wtables,
                        face,
                        &mut res,
                        &propa_sub_info,
                        zid,
                    );
                }
                WhatAmI::Peer => {
                    if wtables.hat.full_net(WhatAmI::Peer) {
                        let zid = wtables.zid;
                        register_peer_subscription(
                            &mut wtables,
                            face,
                            &mut res,
                            &propa_sub_info,
                            zid,
                        );
                    } else {
                        propagate_simple_subscription(&mut wtables, &res, &propa_sub_info, face);
                        // This introduced a buffer overflow on windows
                        // TODO: Let's deactivate this on windows until Fixed
                        #[cfg(not(windows))]
                        for mcast_group in &wtables.mcast_groups {
                            mcast_group.primitives.send_declare(Declare {
                                ext_qos: ext::QoSType::declare_default(),
                                ext_tstamp: None,
                                ext_nodeid: ext::NodeIdType::default(),
                                body: DeclareBody::DeclareSubscriber(DeclareSubscriber {
                                    id: 0, // TODO
                                    wire_expr: res.expr().into(),
                                    ext_info: *sub_info,
                                }),
                            })
                        }
                    }
                }
                _ => {
                    propagate_simple_subscription(&mut wtables, &res, &propa_sub_info, face);
                    // This introduced a buffer overflow on windows
                    // TODO: Let's deactivate this on windows until Fixed
                    #[cfg(not(windows))]
                    for mcast_group in &wtables.mcast_groups {
                        mcast_group.primitives.send_declare(Declare {
                            ext_qos: ext::QoSType::declare_default(),
                            ext_tstamp: None,
                            ext_nodeid: ext::NodeIdType::default(),
                            body: DeclareBody::DeclareSubscriber(DeclareSubscriber {
                                id: 0, // TODO
                                wire_expr: res.expr().into(),
                                ext_info: *sub_info,
                            }),
                        })
                    }
                }
            }
            disable_matches_data_routes(&mut wtables, &mut res);
            drop(wtables);

            let rtables = zread!(tables.tables);
            let matches_data_routes = compute_matches_data_routes_(&rtables, &res);
            drop(rtables);

            let wtables = zwrite!(tables.tables);
            for (mut res, data_routes) in matches_data_routes {
                get_mut_unchecked(&mut res)
                    .context_mut()
                    .update_data_routes(data_routes);
            }
            drop(wtables);
        }
        None => log::error!("Declare subscription for unknown scope {}!", expr.scope),
    }
}

#[inline]
fn remote_router_subs(tables: &Tables, res: &Arc<Resource>) -> bool {
    res.context.is_some()
        && res
            .context()
            .router_subs
            .iter()
            .any(|peer| peer != &tables.zid)
}

#[inline]
fn remote_peer_subs(tables: &Tables, res: &Arc<Resource>) -> bool {
    res.context.is_some()
        && res
            .context()
            .peer_subs
            .iter()
            .any(|peer| peer != &tables.zid)
}

#[inline]
fn client_subs(res: &Arc<Resource>) -> Vec<Arc<FaceState>> {
    res.session_ctxs
        .values()
        .filter_map(|ctx| {
            if ctx.subs.is_some() {
                Some(ctx.face.clone())
            } else {
                None
            }
        })
        .collect()
}

#[inline]
fn send_forget_sourced_subscription_to_net_childs(
    tables: &Tables,
    net: &Network,
    childs: &[NodeIndex],
    res: &Arc<Resource>,
    src_face: Option<&Arc<FaceState>>,
    routing_context: Option<RoutingContext>,
) {
    for child in childs {
        if net.graph.contains_node(*child) {
            match tables.get_face(&net.graph[*child].zid).cloned() {
                Some(mut someface) => {
                    if src_face.is_none() || someface.id != src_face.unwrap().id {
                        let wire_expr = Resource::decl_key(res, &mut someface);

                        log::debug!("Send forget subscription {} on {}", res.expr(), someface);

                        someface.primitives.send_declare(Declare {
                            ext_qos: ext::QoSType::declare_default(),
                            ext_tstamp: None,
                            ext_nodeid: ext::NodeIdType {
                                node_id: routing_context.unwrap_or(0),
                            },
                            body: DeclareBody::UndeclareSubscriber(UndeclareSubscriber {
                                id: 0, // TODO
                                ext_wire_expr: WireExprType { wire_expr },
                            }),
                        });
                    }
                }
                None => log::trace!("Unable to find face for zid {}", net.graph[*child].zid),
            }
        }
    }
}

fn propagate_forget_simple_subscription(tables: &mut Tables, res: &Arc<Resource>) {
    for face in tables.faces.values_mut() {
        if face.local_subs.contains(res) {
            let wire_expr = Resource::get_best_key(res, "", face.id);
            face.primitives.send_declare(Declare {
                ext_qos: ext::QoSType::declare_default(),
                ext_tstamp: None,
                ext_nodeid: ext::NodeIdType::default(),
                body: DeclareBody::UndeclareSubscriber(UndeclareSubscriber {
                    id: 0, // TODO
                    ext_wire_expr: WireExprType { wire_expr },
                }),
            });
            get_mut_unchecked(face).local_subs.remove(res);
        }
    }
}

fn propagate_forget_simple_subscription_to_peers(tables: &mut Tables, res: &Arc<Resource>) {
    if !tables.hat.full_net(WhatAmI::Peer)
        && res.context().router_subs.len() == 1
        && res.context().router_subs.contains(&tables.zid)
    {
        for mut face in tables
            .faces
            .values()
            .cloned()
            .collect::<Vec<Arc<FaceState>>>()
        {
            if face.whatami == WhatAmI::Peer
                && face.local_subs.contains(res)
                && !res.session_ctxs.values().any(|s| {
                    face.zid != s.face.zid
                        && s.subs.is_some()
                        && (s.face.whatami == WhatAmI::Client
                            || (s.face.whatami == WhatAmI::Peer
                                && tables.hat.failover_brokering(s.face.zid, face.zid)))
                })
            {
                let wire_expr = Resource::get_best_key(res, "", face.id);
                face.primitives.send_declare(Declare {
                    ext_qos: ext::QoSType::declare_default(),
                    ext_tstamp: None,
                    ext_nodeid: ext::NodeIdType::default(),
                    body: DeclareBody::UndeclareSubscriber(UndeclareSubscriber {
                        id: 0, // TODO
                        ext_wire_expr: WireExprType { wire_expr },
                    }),
                });

                get_mut_unchecked(&mut face).local_subs.remove(res);
            }
        }
    }
}

fn propagate_forget_sourced_subscription(
    tables: &Tables,
    res: &Arc<Resource>,
    src_face: Option<&Arc<FaceState>>,
    source: &ZenohId,
    net_type: WhatAmI,
) {
    let net = tables.hat.get_net(net_type).unwrap();
    match net.get_idx(source) {
        Some(tree_sid) => {
            if net.trees.len() > tree_sid.index() {
                send_forget_sourced_subscription_to_net_childs(
                    tables,
                    net,
                    &net.trees[tree_sid.index()].childs,
                    res,
                    src_face,
                    Some(tree_sid.index() as u16),
                );
            } else {
                log::trace!(
                    "Propagating forget sub {}: tree for node {} sid:{} not yet ready",
                    res.expr(),
                    tree_sid.index(),
                    source
                );
            }
        }
        None => log::error!(
            "Error propagating forget sub {}: cannot get index of {}!",
            res.expr(),
            source
        ),
    }
}

fn unregister_router_subscription(tables: &mut Tables, res: &mut Arc<Resource>, router: &ZenohId) {
    log::debug!(
        "Unregister router subscription {} (router: {})",
        res.expr(),
        router
    );
    get_mut_unchecked(res)
        .context_mut()
        .router_subs
        .retain(|sub| sub != router);

    if res.context().router_subs.is_empty() {
        tables.hat.router_subs.retain(|sub| !Arc::ptr_eq(sub, res));

        if tables.hat.full_net(WhatAmI::Peer) {
            undeclare_peer_subscription(tables, None, res, &tables.zid.clone());
        }
        propagate_forget_simple_subscription(tables, res);
    }

    propagate_forget_simple_subscription_to_peers(tables, res);
}

fn undeclare_router_subscription(
    tables: &mut Tables,
    face: Option<&Arc<FaceState>>,
    res: &mut Arc<Resource>,
    router: &ZenohId,
) {
    if res.context().router_subs.contains(router) {
        unregister_router_subscription(tables, res, router);
        propagate_forget_sourced_subscription(tables, res, face, router, WhatAmI::Router);
    }
}

pub fn forget_router_subscription(
    tables: &TablesLock,
    rtables: RwLockReadGuard<Tables>,
    face: &mut Arc<FaceState>,
    expr: &WireExpr,
    router: &ZenohId,
) {
    match rtables.get_mapping(face, &expr.scope, expr.mapping) {
        Some(prefix) => match Resource::get_resource(prefix, expr.suffix.as_ref()) {
            Some(mut res) => {
                drop(rtables);
                let mut wtables = zwrite!(tables.tables);
                undeclare_router_subscription(&mut wtables, Some(face), &mut res, router);
                disable_matches_data_routes(&mut wtables, &mut res);
                drop(wtables);

                let rtables = zread!(tables.tables);
                let matches_data_routes = compute_matches_data_routes_(&rtables, &res);
                drop(rtables);
                let wtables = zwrite!(tables.tables);
                for (mut res, data_routes) in matches_data_routes {
                    get_mut_unchecked(&mut res)
                        .context_mut()
                        .update_data_routes(data_routes);
                }
                Resource::clean(&mut res);
                drop(wtables);
            }
            None => log::error!("Undeclare unknown router subscription!"),
        },
        None => log::error!("Undeclare router subscription with unknown scope!"),
    }
}

fn unregister_peer_subscription(tables: &mut Tables, res: &mut Arc<Resource>, peer: &ZenohId) {
    log::debug!(
        "Unregister peer subscription {} (peer: {})",
        res.expr(),
        peer
    );
    get_mut_unchecked(res)
        .context_mut()
        .peer_subs
        .retain(|sub| sub != peer);

    if res.context().peer_subs.is_empty() {
        tables.hat.peer_subs.retain(|sub| !Arc::ptr_eq(sub, res));

        if tables.whatami == WhatAmI::Peer {
            propagate_forget_simple_subscription(tables, res);
        }
    }
}

fn undeclare_peer_subscription(
    tables: &mut Tables,
    face: Option<&Arc<FaceState>>,
    res: &mut Arc<Resource>,
    peer: &ZenohId,
) {
    if res.context().peer_subs.contains(peer) {
        unregister_peer_subscription(tables, res, peer);
        propagate_forget_sourced_subscription(tables, res, face, peer, WhatAmI::Peer);
    }
}

pub fn forget_peer_subscription(
    tables: &TablesLock,
    rtables: RwLockReadGuard<Tables>,
    face: &mut Arc<FaceState>,
    expr: &WireExpr,
    peer: &ZenohId,
) {
    match rtables.get_mapping(face, &expr.scope, expr.mapping) {
        Some(prefix) => match Resource::get_resource(prefix, expr.suffix.as_ref()) {
            Some(mut res) => {
                drop(rtables);
                let mut wtables = zwrite!(tables.tables);
                undeclare_peer_subscription(&mut wtables, Some(face), &mut res, peer);
                if wtables.whatami == WhatAmI::Router {
                    let client_subs = res.session_ctxs.values().any(|ctx| ctx.subs.is_some());
                    let peer_subs = remote_peer_subs(&wtables, &res);
                    let zid = wtables.zid;
                    if !client_subs && !peer_subs {
                        undeclare_router_subscription(&mut wtables, None, &mut res, &zid);
                    }
                }
                disable_matches_data_routes(&mut wtables, &mut res);
                drop(wtables);

                let rtables = zread!(tables.tables);
                let matches_data_routes = compute_matches_data_routes_(&rtables, &res);
                drop(rtables);
                let wtables = zwrite!(tables.tables);
                for (mut res, data_routes) in matches_data_routes {
                    get_mut_unchecked(&mut res)
                        .context_mut()
                        .update_data_routes(data_routes);
                }
                Resource::clean(&mut res);
                drop(wtables);
            }
            None => log::error!("Undeclare unknown peer subscription!"),
        },
        None => log::error!("Undeclare peer subscription with unknown scope!"),
    }
}

pub(crate) fn undeclare_client_subscription(
    tables: &mut Tables,
    face: &mut Arc<FaceState>,
    res: &mut Arc<Resource>,
) {
    log::debug!("Unregister client subscription {} for {}", res.expr(), face);
    if let Some(ctx) = get_mut_unchecked(res).session_ctxs.get_mut(&face.id) {
        get_mut_unchecked(ctx).subs = None;
    }
    get_mut_unchecked(face).remote_subs.remove(res);

    let mut client_subs = client_subs(res);
    let router_subs = remote_router_subs(tables, res);
    let peer_subs = remote_peer_subs(tables, res);
    match tables.whatami {
        WhatAmI::Router => {
            if client_subs.is_empty() && !peer_subs {
                undeclare_router_subscription(tables, None, res, &tables.zid.clone());
            } else {
                propagate_forget_simple_subscription_to_peers(tables, res);
            }
        }
        WhatAmI::Peer => {
            if client_subs.is_empty() {
                if tables.hat.full_net(WhatAmI::Peer) {
                    undeclare_peer_subscription(tables, None, res, &tables.zid.clone());
                } else {
                    propagate_forget_simple_subscription(tables, res);
                }
            }
        }
        _ => {
            if client_subs.is_empty() {
                propagate_forget_simple_subscription(tables, res);
            }
        }
    }
    if client_subs.len() == 1 && !router_subs && !peer_subs {
        let face = &mut client_subs[0];
        if face.local_subs.contains(res)
            && !(face.whatami == WhatAmI::Client && res.expr().starts_with(PREFIX_LIVELINESS))
        {
            let wire_expr = Resource::get_best_key(res, "", face.id);
            face.primitives.send_declare(Declare {
                ext_qos: ext::QoSType::declare_default(),
                ext_tstamp: None,
                ext_nodeid: ext::NodeIdType::default(),
                body: DeclareBody::UndeclareSubscriber(UndeclareSubscriber {
                    id: 0, // TODO
                    ext_wire_expr: WireExprType { wire_expr },
                }),
            });

            get_mut_unchecked(face).local_subs.remove(res);
        }
    }
}

pub fn forget_client_subscription(
    tables: &TablesLock,
    rtables: RwLockReadGuard<Tables>,
    face: &mut Arc<FaceState>,
    expr: &WireExpr,
) {
    match rtables.get_mapping(face, &expr.scope, expr.mapping) {
        Some(prefix) => match Resource::get_resource(prefix, expr.suffix.as_ref()) {
            Some(mut res) => {
                drop(rtables);
                let mut wtables = zwrite!(tables.tables);
                undeclare_client_subscription(&mut wtables, face, &mut res);
                disable_matches_data_routes(&mut wtables, &mut res);
                drop(wtables);

                let rtables = zread!(tables.tables);
                let matches_data_routes = compute_matches_data_routes_(&rtables, &res);
                drop(rtables);

                let wtables = zwrite!(tables.tables);
                for (mut res, data_routes) in matches_data_routes {
                    get_mut_unchecked(&mut res)
                        .context_mut()
                        .update_data_routes(data_routes);
                }
                Resource::clean(&mut res);
                drop(wtables);
            }
            None => log::error!("Undeclare unknown subscription!"),
        },
        None => log::error!("Undeclare subscription with unknown scope!"),
    }
}

pub(crate) fn pubsub_new_face(tables: &mut Tables, face: &mut Arc<FaceState>) {
    let sub_info = SubscriberInfo {
        reliability: Reliability::Reliable, // @TODO
        mode: Mode::Push,
    };
    match tables.whatami {
        WhatAmI::Router => {
            if face.whatami == WhatAmI::Client {
                for sub in &tables.hat.router_subs {
                    get_mut_unchecked(face).local_subs.insert(sub.clone());
                    let key_expr = Resource::decl_key(sub, face);
                    face.primitives.send_declare(Declare {
                        ext_qos: ext::QoSType::declare_default(),
                        ext_tstamp: None,
                        ext_nodeid: ext::NodeIdType::default(),
                        body: DeclareBody::DeclareSubscriber(DeclareSubscriber {
                            id: 0, // TODO
                            wire_expr: key_expr,
                            ext_info: sub_info,
                        }),
                    });
                }
            } else if face.whatami == WhatAmI::Peer && !tables.hat.full_net(WhatAmI::Peer) {
                for sub in &tables.hat.router_subs {
                    if sub.context.is_some()
                        && (sub.context().router_subs.iter().any(|r| *r != tables.zid)
                            || sub.session_ctxs.values().any(|s| {
                                s.subs.is_some()
                                    && (s.face.whatami == WhatAmI::Client
                                        || (s.face.whatami == WhatAmI::Peer
                                            && tables.hat.failover_brokering(s.face.zid, face.zid)))
                            }))
                    {
                        get_mut_unchecked(face).local_subs.insert(sub.clone());
                        let key_expr = Resource::decl_key(sub, face);
                        face.primitives.send_declare(Declare {
                            ext_qos: ext::QoSType::declare_default(),
                            ext_tstamp: None,
                            ext_nodeid: ext::NodeIdType::default(),
                            body: DeclareBody::DeclareSubscriber(DeclareSubscriber {
                                id: 0, // TODO
                                wire_expr: key_expr,
                                ext_info: sub_info,
                            }),
                        });
                    }
                }
            }
        }
        WhatAmI::Peer => {
            if tables.hat.full_net(WhatAmI::Peer) {
                if face.whatami == WhatAmI::Client {
                    for sub in &tables.hat.peer_subs {
                        get_mut_unchecked(face).local_subs.insert(sub.clone());
                        let key_expr = Resource::decl_key(sub, face);
                        face.primitives.send_declare(Declare {
                            ext_qos: ext::QoSType::declare_default(),
                            ext_tstamp: None,
                            ext_nodeid: ext::NodeIdType::default(),
                            body: DeclareBody::DeclareSubscriber(DeclareSubscriber {
                                id: 0, // TODO
                                wire_expr: key_expr,
                                ext_info: sub_info,
                            }),
                        });
                    }
                }
            } else {
                for src_face in tables
                    .faces
                    .values()
                    .cloned()
                    .collect::<Vec<Arc<FaceState>>>()
                {
                    for sub in &src_face.remote_subs {
                        propagate_simple_subscription_to(
                            tables,
                            face,
                            sub,
                            &sub_info,
                            &mut src_face.clone(),
                            false,
                        );
                    }
                }
            }
        }
        WhatAmI::Client => {
            for src_face in tables
                .faces
                .values()
                .cloned()
                .collect::<Vec<Arc<FaceState>>>()
            {
                for sub in &src_face.remote_subs {
                    propagate_simple_subscription_to(
                        tables,
                        face,
                        sub,
                        &sub_info,
                        &mut src_face.clone(),
                        false,
                    );
                }
            }
        }
    }
}

pub(crate) fn pubsub_remove_node(tables: &mut Tables, node: &ZenohId, net_type: WhatAmI) {
    match net_type {
        WhatAmI::Router => {
            for mut res in tables
                .hat
                .router_subs
                .iter()
                .filter(|res| res.context().router_subs.contains(node))
                .cloned()
                .collect::<Vec<Arc<Resource>>>()
            {
                unregister_router_subscription(tables, &mut res, node);

                let matches_data_routes = compute_matches_data_routes_(tables, &res);
                for (mut res, data_routes) in matches_data_routes {
                    get_mut_unchecked(&mut res)
                        .context_mut()
                        .update_data_routes(data_routes);
                }
                Resource::clean(&mut res)
            }
        }
        WhatAmI::Peer => {
            for mut res in tables
                .hat
                .peer_subs
                .iter()
                .filter(|res| res.context().peer_subs.contains(node))
                .cloned()
                .collect::<Vec<Arc<Resource>>>()
            {
                unregister_peer_subscription(tables, &mut res, node);

                if tables.whatami == WhatAmI::Router {
                    let client_subs = res.session_ctxs.values().any(|ctx| ctx.subs.is_some());
                    let peer_subs = remote_peer_subs(tables, &res);
                    if !client_subs && !peer_subs {
                        undeclare_router_subscription(tables, None, &mut res, &tables.zid.clone());
                    }
                }

                // compute_matches_data_routes(tables, &mut res);
                let matches_data_routes = compute_matches_data_routes_(tables, &res);
                for (mut res, data_routes) in matches_data_routes {
                    get_mut_unchecked(&mut res)
                        .context_mut()
                        .update_data_routes(data_routes);
                }
                Resource::clean(&mut res)
            }
        }
        _ => (),
    }
}

pub(crate) fn pubsub_tree_change(
    tables: &mut Tables,
    new_childs: &[Vec<NodeIndex>],
    net_type: WhatAmI,
) {
    // propagate subs to new childs
    for (tree_sid, tree_childs) in new_childs.iter().enumerate() {
        if !tree_childs.is_empty() {
            let net = tables.hat.get_net(net_type).unwrap();
            let tree_idx = NodeIndex::new(tree_sid);
            if net.graph.contains_node(tree_idx) {
                let tree_id = net.graph[tree_idx].zid;

                let subs_res = match net_type {
                    WhatAmI::Router => &tables.hat.router_subs,
                    _ => &tables.hat.peer_subs,
                };

                for res in subs_res {
                    let subs = match net_type {
                        WhatAmI::Router => &res.context().router_subs,
                        _ => &res.context().peer_subs,
                    };
                    for sub in subs {
                        if *sub == tree_id {
                            let sub_info = SubscriberInfo {
                                reliability: Reliability::Reliable, // @TODO
                                mode: Mode::Push,
                            };
                            send_sourced_subscription_to_net_childs(
                                tables,
                                net,
                                tree_childs,
                                res,
                                None,
                                &sub_info,
                                Some(tree_sid as u16),
                            );
                        }
                    }
                }
            }
        }
    }

    // recompute routes
    compute_data_routes_from(tables, &mut tables.root_res.clone());
}

pub(crate) fn pubsub_linkstate_change(tables: &mut Tables, zid: &ZenohId, links: &[ZenohId]) {
    if let Some(src_face) = tables.get_face(zid).cloned() {
        if tables.hat.router_peers_failover_brokering
            && tables.whatami == WhatAmI::Router
            && src_face.whatami == WhatAmI::Peer
        {
            for res in &src_face.remote_subs {
                let client_subs = res
                    .session_ctxs
                    .values()
                    .any(|ctx| ctx.face.whatami == WhatAmI::Client && ctx.subs.is_some());
                if !remote_router_subs(tables, res) && !client_subs {
                    for ctx in get_mut_unchecked(&mut res.clone())
                        .session_ctxs
                        .values_mut()
                    {
                        let dst_face = &mut get_mut_unchecked(ctx).face;
                        if dst_face.whatami == WhatAmI::Peer && src_face.zid != dst_face.zid {
                            if dst_face.local_subs.contains(res) {
                                let forget = !HatTables::failover_brokering_to(links, dst_face.zid)
                                    && {
                                        let ctx_links = tables
                                            .hat
                                            .peers_net
                                            .as_ref()
                                            .map(|net| net.get_links(dst_face.zid))
                                            .unwrap_or_else(|| &[]);
                                        res.session_ctxs.values().any(|ctx2| {
                                            ctx2.face.whatami == WhatAmI::Peer
                                                && ctx2.subs.is_some()
                                                && HatTables::failover_brokering_to(
                                                    ctx_links,
                                                    ctx2.face.zid,
                                                )
                                        })
                                    };
                                if forget {
                                    let wire_expr = Resource::get_best_key(res, "", dst_face.id);
                                    dst_face.primitives.send_declare(Declare {
                                        ext_qos: ext::QoSType::declare_default(),
                                        ext_tstamp: None,
                                        ext_nodeid: ext::NodeIdType::default(),
                                        body: DeclareBody::UndeclareSubscriber(
                                            UndeclareSubscriber {
                                                id: 0, // TODO
                                                ext_wire_expr: WireExprType { wire_expr },
                                            },
                                        ),
                                    });

                                    get_mut_unchecked(dst_face).local_subs.remove(res);
                                }
                            } else if HatTables::failover_brokering_to(links, ctx.face.zid) {
                                let dst_face = &mut get_mut_unchecked(ctx).face;
                                get_mut_unchecked(dst_face).local_subs.insert(res.clone());
                                let key_expr = Resource::decl_key(res, dst_face);
                                let sub_info = SubscriberInfo {
                                    reliability: Reliability::Reliable, // TODO
                                    mode: Mode::Push,
                                };
                                dst_face.primitives.send_declare(Declare {
                                    ext_qos: ext::QoSType::declare_default(),
                                    ext_tstamp: None,
                                    ext_nodeid: ext::NodeIdType::default(),
                                    body: DeclareBody::DeclareSubscriber(DeclareSubscriber {
                                        id: 0, // TODO
                                        wire_expr: key_expr,
                                        ext_info: sub_info,
                                    }),
                                });
                            }
                        }
                    }
                }
            }
        }
    }
}