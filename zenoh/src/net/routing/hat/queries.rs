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
use super::super::dispatcher::queries::*;
use super::super::dispatcher::resource::{Resource, RoutingContext, SessionContext};
use super::super::dispatcher::tables::{Tables, TablesLock};
use super::network::Network;
use super::HatTables;
use petgraph::graph::NodeIndex;
use std::collections::HashMap;
use std::sync::{Arc, RwLockReadGuard};
use zenoh_protocol::{
    core::{key_expr::keyexpr, WhatAmI, WireExpr, ZenohId},
    network::declare::{
        common::ext::WireExprType, ext, queryable::ext::QueryableInfo, Declare, DeclareBody,
        DeclareQueryable, UndeclareQueryable,
    },
};
use zenoh_sync::get_mut_unchecked;

#[cfg(feature = "complete_n")]
#[inline]
fn merge_qabl_infos(mut this: QueryableInfo, info: &QueryableInfo) -> QueryableInfo {
    this.complete += info.complete;
    this.distance = std::cmp::min(this.distance, info.distance);
    this
}

#[cfg(not(feature = "complete_n"))]
#[inline]
fn merge_qabl_infos(mut this: QueryableInfo, info: &QueryableInfo) -> QueryableInfo {
    this.complete = u8::from(this.complete != 0 || info.complete != 0);
    this.distance = std::cmp::min(this.distance, info.distance);
    this
}

fn local_router_qabl_info(tables: &Tables, res: &Arc<Resource>) -> QueryableInfo {
    let info = if tables.hat.full_net(WhatAmI::Peer) {
        res.context.as_ref().and_then(|ctx| {
            ctx.peer_qabls.iter().fold(None, |accu, (zid, info)| {
                if *zid != tables.zid {
                    Some(match accu {
                        Some(accu) => merge_qabl_infos(accu, info),
                        None => *info,
                    })
                } else {
                    accu
                }
            })
        })
    } else {
        None
    };
    res.session_ctxs
        .values()
        .fold(info, |accu, ctx| {
            if let Some(info) = ctx.qabl.as_ref() {
                Some(match accu {
                    Some(accu) => merge_qabl_infos(accu, info),
                    None => *info,
                })
            } else {
                accu
            }
        })
        .unwrap_or(QueryableInfo {
            complete: 0,
            distance: 0,
        })
}

fn local_peer_qabl_info(tables: &Tables, res: &Arc<Resource>) -> QueryableInfo {
    let info = if tables.whatami == WhatAmI::Router && res.context.is_some() {
        res.context()
            .router_qabls
            .iter()
            .fold(None, |accu, (zid, info)| {
                if *zid != tables.zid {
                    Some(match accu {
                        Some(accu) => merge_qabl_infos(accu, info),
                        None => *info,
                    })
                } else {
                    accu
                }
            })
    } else {
        None
    };
    res.session_ctxs
        .values()
        .fold(info, |accu, ctx| {
            if let Some(info) = ctx.qabl.as_ref() {
                Some(match accu {
                    Some(accu) => merge_qabl_infos(accu, info),
                    None => *info,
                })
            } else {
                accu
            }
        })
        .unwrap_or(QueryableInfo {
            complete: 0,
            distance: 0,
        })
}

fn local_qabl_info(tables: &Tables, res: &Arc<Resource>, face: &Arc<FaceState>) -> QueryableInfo {
    let mut info = if tables.whatami == WhatAmI::Router && res.context.is_some() {
        res.context()
            .router_qabls
            .iter()
            .fold(None, |accu, (zid, info)| {
                if *zid != tables.zid {
                    Some(match accu {
                        Some(accu) => merge_qabl_infos(accu, info),
                        None => *info,
                    })
                } else {
                    accu
                }
            })
    } else {
        None
    };
    if res.context.is_some() && tables.hat.full_net(WhatAmI::Peer) {
        info = res
            .context()
            .peer_qabls
            .iter()
            .fold(info, |accu, (zid, info)| {
                if *zid != tables.zid {
                    Some(match accu {
                        Some(accu) => merge_qabl_infos(accu, info),
                        None => *info,
                    })
                } else {
                    accu
                }
            })
    }
    res.session_ctxs
        .values()
        .fold(info, |accu, ctx| {
            if ctx.face.id != face.id && ctx.face.whatami != WhatAmI::Peer
                || face.whatami != WhatAmI::Peer
                || tables.hat.failover_brokering(ctx.face.zid, face.zid)
            {
                if let Some(info) = ctx.qabl.as_ref() {
                    Some(match accu {
                        Some(accu) => merge_qabl_infos(accu, info),
                        None => *info,
                    })
                } else {
                    accu
                }
            } else {
                accu
            }
        })
        .unwrap_or(QueryableInfo {
            complete: 0,
            distance: 0,
        })
}

#[allow(clippy::too_many_arguments)]
#[inline]
fn send_sourced_queryable_to_net_childs(
    tables: &Tables,
    net: &Network,
    childs: &[NodeIndex],
    res: &Arc<Resource>,
    qabl_info: &QueryableInfo,
    src_face: Option<&mut Arc<FaceState>>,
    routing_context: Option<RoutingContext>,
) {
    for child in childs {
        if net.graph.contains_node(*child) {
            match tables.get_face(&net.graph[*child].zid).cloned() {
                Some(mut someface) => {
                    if src_face.is_none() || someface.id != src_face.as_ref().unwrap().id {
                        let key_expr = Resource::decl_key(res, &mut someface);

                        log::debug!("Send queryable {} on {}", res.expr(), someface);

                        someface.primitives.send_declare(Declare {
                            ext_qos: ext::QoSType::declare_default(),
                            ext_tstamp: None,
                            ext_nodeid: ext::NodeIdType {
                                node_id: routing_context.unwrap_or(0),
                            },
                            body: DeclareBody::DeclareQueryable(DeclareQueryable {
                                id: 0, // TODO
                                wire_expr: key_expr,
                                ext_info: *qabl_info,
                            }),
                        });
                    }
                }
                None => log::trace!("Unable to find face for zid {}", net.graph[*child].zid),
            }
        }
    }
}

fn propagate_simple_queryable(
    tables: &mut Tables,
    res: &Arc<Resource>,
    src_face: Option<&mut Arc<FaceState>>,
) {
    let full_peers_net = tables.hat.full_net(WhatAmI::Peer);
    let faces = tables.faces.values().cloned();
    for mut dst_face in faces {
        let info = local_qabl_info(tables, res, &dst_face);
        let current_info = dst_face.local_qabls.get(res);
        if (src_face.is_none() || src_face.as_ref().unwrap().id != dst_face.id)
            && (current_info.is_none() || *current_info.unwrap() != info)
            && match tables.whatami {
                WhatAmI::Router => {
                    if full_peers_net {
                        dst_face.whatami == WhatAmI::Client
                    } else {
                        dst_face.whatami != WhatAmI::Router
                            && (src_face.is_none()
                                || src_face.as_ref().unwrap().whatami != WhatAmI::Peer
                                || dst_face.whatami != WhatAmI::Peer
                                || tables.hat.failover_brokering(
                                    src_face.as_ref().unwrap().zid,
                                    dst_face.zid,
                                ))
                    }
                }
                WhatAmI::Peer => {
                    if full_peers_net {
                        dst_face.whatami == WhatAmI::Client
                    } else {
                        src_face.is_none()
                            || src_face.as_ref().unwrap().whatami == WhatAmI::Client
                            || dst_face.whatami == WhatAmI::Client
                    }
                }
                _ => {
                    src_face.is_none()
                        || src_face.as_ref().unwrap().whatami == WhatAmI::Client
                        || dst_face.whatami == WhatAmI::Client
                }
            }
        {
            get_mut_unchecked(&mut dst_face)
                .local_qabls
                .insert(res.clone(), info);
            let key_expr = Resource::decl_key(res, &mut dst_face);
            dst_face.primitives.send_declare(Declare {
                ext_qos: ext::QoSType::declare_default(),
                ext_tstamp: None,
                ext_nodeid: ext::NodeIdType::default(),
                body: DeclareBody::DeclareQueryable(DeclareQueryable {
                    id: 0, // TODO
                    wire_expr: key_expr,
                    ext_info: info,
                }),
            });
        }
    }
}

fn propagate_sourced_queryable(
    tables: &Tables,
    res: &Arc<Resource>,
    qabl_info: &QueryableInfo,
    src_face: Option<&mut Arc<FaceState>>,
    source: &ZenohId,
    net_type: WhatAmI,
) {
    let net = tables.hat.get_net(net_type).unwrap();
    match net.get_idx(source) {
        Some(tree_sid) => {
            if net.trees.len() > tree_sid.index() {
                send_sourced_queryable_to_net_childs(
                    tables,
                    net,
                    &net.trees[tree_sid.index()].childs,
                    res,
                    qabl_info,
                    src_face,
                    Some(tree_sid.index() as u16),
                );
            } else {
                log::trace!(
                    "Propagating qabl {}: tree for node {} sid:{} not yet ready",
                    res.expr(),
                    tree_sid.index(),
                    source
                );
            }
        }
        None => log::error!(
            "Error propagating qabl {}: cannot get index of {}!",
            res.expr(),
            source
        ),
    }
}

fn register_router_queryable(
    tables: &mut Tables,
    mut face: Option<&mut Arc<FaceState>>,
    res: &mut Arc<Resource>,
    qabl_info: &QueryableInfo,
    router: ZenohId,
) {
    let current_info = res.context().router_qabls.get(&router);
    if current_info.is_none() || current_info.unwrap() != qabl_info {
        // Register router queryable
        {
            log::debug!(
                "Register router queryable {} (router: {})",
                res.expr(),
                router,
            );
            get_mut_unchecked(res)
                .context_mut()
                .router_qabls
                .insert(router, *qabl_info);
            tables.hat.router_qabls.insert(res.clone());
        }

        // Propagate queryable to routers
        propagate_sourced_queryable(
            tables,
            res,
            qabl_info,
            face.as_deref_mut(),
            &router,
            WhatAmI::Router,
        );
    }

    if tables.hat.full_net(WhatAmI::Peer) {
        // Propagate queryable to peers
        if face.is_none() || face.as_ref().unwrap().whatami != WhatAmI::Peer {
            let local_info = local_peer_qabl_info(tables, res);
            register_peer_queryable(tables, face.as_deref_mut(), res, &local_info, tables.zid)
        }
    }

    // Propagate queryable to clients
    propagate_simple_queryable(tables, res, face);
}

pub fn declare_router_queryable(
    tables: &TablesLock,
    rtables: RwLockReadGuard<Tables>,
    face: &mut Arc<FaceState>,
    expr: &WireExpr,
    qabl_info: &QueryableInfo,
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
                    log::debug!("Register router queryable {}", fullexpr);
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
            register_router_queryable(&mut wtables, Some(face), &mut res, qabl_info, router);
            disable_matches_query_routes(&mut wtables, &mut res);
            drop(wtables);

            let rtables = zread!(tables.tables);
            let matches_query_routes = compute_matches_query_routes_(&rtables, &res);
            drop(rtables);

            let wtables = zwrite!(tables.tables);
            for (mut res, query_routes) in matches_query_routes {
                get_mut_unchecked(&mut res)
                    .context_mut()
                    .update_query_routes(query_routes);
            }
            drop(wtables);
        }
        None => log::error!("Declare router queryable for unknown scope {}!", expr.scope),
    }
}

fn register_peer_queryable(
    tables: &mut Tables,
    mut face: Option<&mut Arc<FaceState>>,
    res: &mut Arc<Resource>,
    qabl_info: &QueryableInfo,
    peer: ZenohId,
) {
    let current_info = res.context().peer_qabls.get(&peer);
    if current_info.is_none() || current_info.unwrap() != qabl_info {
        // Register peer queryable
        {
            log::debug!("Register peer queryable {} (peer: {})", res.expr(), peer,);
            get_mut_unchecked(res)
                .context_mut()
                .peer_qabls
                .insert(peer, *qabl_info);
            tables.hat.peer_qabls.insert(res.clone());
        }

        // Propagate queryable to peers
        propagate_sourced_queryable(
            tables,
            res,
            qabl_info,
            face.as_deref_mut(),
            &peer,
            WhatAmI::Peer,
        );
    }

    if tables.whatami == WhatAmI::Peer {
        // Propagate queryable to clients
        propagate_simple_queryable(tables, res, face);
    }
}

pub fn declare_peer_queryable(
    tables: &TablesLock,
    rtables: RwLockReadGuard<Tables>,
    face: &mut Arc<FaceState>,
    expr: &WireExpr,
    qabl_info: &QueryableInfo,
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
                    log::debug!("Register peer queryable {}", fullexpr);
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
            let mut face = Some(face);
            register_peer_queryable(&mut wtables, face.as_deref_mut(), &mut res, qabl_info, peer);
            if wtables.whatami == WhatAmI::Router {
                let local_info = local_router_qabl_info(&wtables, &res);
                let zid = wtables.zid;
                register_router_queryable(&mut wtables, face, &mut res, &local_info, zid);
            }
            disable_matches_query_routes(&mut wtables, &mut res);
            drop(wtables);

            let rtables = zread!(tables.tables);
            let matches_query_routes = compute_matches_query_routes_(&rtables, &res);
            drop(rtables);

            let wtables = zwrite!(tables.tables);
            for (mut res, query_routes) in matches_query_routes {
                get_mut_unchecked(&mut res)
                    .context_mut()
                    .update_query_routes(query_routes);
            }
            drop(wtables);
        }
        None => log::error!("Declare router queryable for unknown scope {}!", expr.scope),
    }
}

fn register_client_queryable(
    _tables: &mut Tables,
    face: &mut Arc<FaceState>,
    res: &mut Arc<Resource>,
    qabl_info: &QueryableInfo,
) {
    // Register queryable
    {
        let res = get_mut_unchecked(res);
        log::debug!("Register queryable {} (face: {})", res.expr(), face,);
        get_mut_unchecked(res.session_ctxs.entry(face.id).or_insert_with(|| {
            Arc::new(SessionContext {
                face: face.clone(),
                local_expr_id: None,
                remote_expr_id: None,
                subs: None,
                qabl: None,
                last_values: HashMap::new(),
            })
        }))
        .qabl = Some(*qabl_info);
    }
    get_mut_unchecked(face).remote_qabls.insert(res.clone());
}

pub fn declare_client_queryable(
    tables: &TablesLock,
    rtables: RwLockReadGuard<Tables>,
    face: &mut Arc<FaceState>,
    expr: &WireExpr,
    qabl_info: &QueryableInfo,
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
                    log::debug!("Register client queryable {}", fullexpr);
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

            register_client_queryable(&mut wtables, face, &mut res, qabl_info);

            match wtables.whatami {
                WhatAmI::Router => {
                    let local_details = local_router_qabl_info(&wtables, &res);
                    let zid = wtables.zid;
                    register_router_queryable(
                        &mut wtables,
                        Some(face),
                        &mut res,
                        &local_details,
                        zid,
                    );
                }
                WhatAmI::Peer => {
                    if wtables.hat.full_net(WhatAmI::Peer) {
                        let local_details = local_peer_qabl_info(&wtables, &res);
                        let zid = wtables.zid;
                        register_peer_queryable(
                            &mut wtables,
                            Some(face),
                            &mut res,
                            &local_details,
                            zid,
                        );
                    } else {
                        propagate_simple_queryable(&mut wtables, &res, Some(face));
                    }
                }
                _ => {
                    propagate_simple_queryable(&mut wtables, &res, Some(face));
                }
            }
            disable_matches_query_routes(&mut wtables, &mut res);
            drop(wtables);

            let rtables = zread!(tables.tables);
            let matches_query_routes = compute_matches_query_routes_(&rtables, &res);
            drop(rtables);

            let wtables = zwrite!(tables.tables);
            for (mut res, query_routes) in matches_query_routes {
                get_mut_unchecked(&mut res)
                    .context_mut()
                    .update_query_routes(query_routes);
            }
            drop(wtables);
        }
        None => log::error!("Declare queryable for unknown scope {}!", expr.scope),
    }
}

#[inline]
fn remote_router_qabls(tables: &Tables, res: &Arc<Resource>) -> bool {
    res.context.is_some()
        && res
            .context()
            .router_qabls
            .keys()
            .any(|router| router != &tables.zid)
}

#[inline]
fn remote_peer_qabls(tables: &Tables, res: &Arc<Resource>) -> bool {
    res.context.is_some()
        && res
            .context()
            .peer_qabls
            .keys()
            .any(|peer| peer != &tables.zid)
}

#[inline]
fn client_qabls(res: &Arc<Resource>) -> Vec<Arc<FaceState>> {
    res.session_ctxs
        .values()
        .filter_map(|ctx| {
            if ctx.qabl.is_some() {
                Some(ctx.face.clone())
            } else {
                None
            }
        })
        .collect()
}

#[inline]
fn send_forget_sourced_queryable_to_net_childs(
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

                        log::debug!("Send forget queryable {}  on {}", res.expr(), someface);

                        someface.primitives.send_declare(Declare {
                            ext_qos: ext::QoSType::declare_default(),
                            ext_tstamp: None,
                            ext_nodeid: ext::NodeIdType {
                                node_id: routing_context.unwrap_or(0),
                            },
                            body: DeclareBody::UndeclareQueryable(UndeclareQueryable {
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

fn propagate_forget_simple_queryable(tables: &mut Tables, res: &mut Arc<Resource>) {
    for face in tables.faces.values_mut() {
        if face.local_qabls.contains_key(res) {
            let wire_expr = Resource::get_best_key(res, "", face.id);
            face.primitives.send_declare(Declare {
                ext_qos: ext::QoSType::declare_default(),
                ext_tstamp: None,
                ext_nodeid: ext::NodeIdType::default(),
                body: DeclareBody::UndeclareQueryable(UndeclareQueryable {
                    id: 0, // TODO
                    ext_wire_expr: WireExprType { wire_expr },
                }),
            });

            get_mut_unchecked(face).local_qabls.remove(res);
        }
    }
}

fn propagate_forget_simple_queryable_to_peers(tables: &mut Tables, res: &mut Arc<Resource>) {
    if !tables.hat.full_net(WhatAmI::Peer)
        && res.context().router_qabls.len() == 1
        && res.context().router_qabls.contains_key(&tables.zid)
    {
        for mut face in tables
            .faces
            .values()
            .cloned()
            .collect::<Vec<Arc<FaceState>>>()
        {
            if face.whatami == WhatAmI::Peer
                && face.local_qabls.contains_key(res)
                && !res.session_ctxs.values().any(|s| {
                    face.zid != s.face.zid
                        && s.qabl.is_some()
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
                    body: DeclareBody::UndeclareQueryable(UndeclareQueryable {
                        id: 0, // TODO
                        ext_wire_expr: WireExprType { wire_expr },
                    }),
                });

                get_mut_unchecked(&mut face).local_qabls.remove(res);
            }
        }
    }
}

fn propagate_forget_sourced_queryable(
    tables: &mut Tables,
    res: &mut Arc<Resource>,
    src_face: Option<&Arc<FaceState>>,
    source: &ZenohId,
    net_type: WhatAmI,
) {
    let net = tables.hat.get_net(net_type).unwrap();
    match net.get_idx(source) {
        Some(tree_sid) => {
            if net.trees.len() > tree_sid.index() {
                send_forget_sourced_queryable_to_net_childs(
                    tables,
                    net,
                    &net.trees[tree_sid.index()].childs,
                    res,
                    src_face,
                    Some(tree_sid.index() as u16),
                );
            } else {
                log::trace!(
                    "Propagating forget qabl {}: tree for node {} sid:{} not yet ready",
                    res.expr(),
                    tree_sid.index(),
                    source
                );
            }
        }
        None => log::error!(
            "Error propagating forget qabl {}: cannot get index of {}!",
            res.expr(),
            source
        ),
    }
}

fn unregister_router_queryable(tables: &mut Tables, res: &mut Arc<Resource>, router: &ZenohId) {
    log::debug!(
        "Unregister router queryable {} (router: {})",
        res.expr(),
        router,
    );
    get_mut_unchecked(res)
        .context_mut()
        .router_qabls
        .remove(router);

    if res.context().router_qabls.is_empty() {
        tables
            .hat
            .router_qabls
            .retain(|qabl| !Arc::ptr_eq(qabl, res));

        if tables.hat.full_net(WhatAmI::Peer) {
            undeclare_peer_queryable(tables, None, res, &tables.zid.clone());
        }
        propagate_forget_simple_queryable(tables, res);
    }

    propagate_forget_simple_queryable_to_peers(tables, res);
}

fn undeclare_router_queryable(
    tables: &mut Tables,
    face: Option<&Arc<FaceState>>,
    res: &mut Arc<Resource>,
    router: &ZenohId,
) {
    if res.context().router_qabls.contains_key(router) {
        unregister_router_queryable(tables, res, router);
        propagate_forget_sourced_queryable(tables, res, face, router, WhatAmI::Router);
    }
}

pub fn forget_router_queryable(
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
                undeclare_router_queryable(&mut wtables, Some(face), &mut res, router);
                disable_matches_query_routes(&mut wtables, &mut res);
                drop(wtables);

                let rtables = zread!(tables.tables);
                let matches_query_routes = compute_matches_query_routes_(&rtables, &res);
                drop(rtables);

                let wtables = zwrite!(tables.tables);
                for (mut res, query_routes) in matches_query_routes {
                    get_mut_unchecked(&mut res)
                        .context_mut()
                        .update_query_routes(query_routes);
                }
                Resource::clean(&mut res);
                drop(wtables);
            }
            None => log::error!("Undeclare unknown router queryable!"),
        },
        None => log::error!("Undeclare router queryable with unknown scope!"),
    }
}

fn unregister_peer_queryable(tables: &mut Tables, res: &mut Arc<Resource>, peer: &ZenohId) {
    log::debug!("Unregister peer queryable {} (peer: {})", res.expr(), peer,);
    get_mut_unchecked(res).context_mut().peer_qabls.remove(peer);

    if res.context().peer_qabls.is_empty() {
        tables.hat.peer_qabls.retain(|qabl| !Arc::ptr_eq(qabl, res));

        if tables.whatami == WhatAmI::Peer {
            propagate_forget_simple_queryable(tables, res);
        }
    }
}

fn undeclare_peer_queryable(
    tables: &mut Tables,
    face: Option<&Arc<FaceState>>,
    res: &mut Arc<Resource>,
    peer: &ZenohId,
) {
    if res.context().peer_qabls.contains_key(peer) {
        unregister_peer_queryable(tables, res, peer);
        propagate_forget_sourced_queryable(tables, res, face, peer, WhatAmI::Peer);
    }
}

pub fn forget_peer_queryable(
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
                undeclare_peer_queryable(&mut wtables, Some(face), &mut res, peer);

                if wtables.whatami == WhatAmI::Router {
                    let client_qabls = res.session_ctxs.values().any(|ctx| ctx.qabl.is_some());
                    let peer_qabls = remote_peer_qabls(&wtables, &res);
                    let zid = wtables.zid;
                    if !client_qabls && !peer_qabls {
                        undeclare_router_queryable(&mut wtables, None, &mut res, &zid);
                    } else {
                        let local_info = local_router_qabl_info(&wtables, &res);
                        register_router_queryable(&mut wtables, None, &mut res, &local_info, zid);
                    }
                }
                drop(wtables);

                let rtables = zread!(tables.tables);
                let matches_query_routes = compute_matches_query_routes_(&rtables, &res);
                drop(rtables);

                let wtables = zwrite!(tables.tables);
                for (mut res, query_routes) in matches_query_routes {
                    get_mut_unchecked(&mut res)
                        .context_mut()
                        .update_query_routes(query_routes);
                }
                Resource::clean(&mut res);
                drop(wtables);
            }
            None => log::error!("Undeclare unknown peer queryable!"),
        },
        None => log::error!("Undeclare peer queryable with unknown scope!"),
    }
}

pub(crate) fn undeclare_client_queryable(
    tables: &mut Tables,
    face: &mut Arc<FaceState>,
    res: &mut Arc<Resource>,
) {
    log::debug!("Unregister client queryable {} for {}", res.expr(), face);
    if let Some(ctx) = get_mut_unchecked(res).session_ctxs.get_mut(&face.id) {
        get_mut_unchecked(ctx).qabl = None;
        if ctx.qabl.is_none() {
            get_mut_unchecked(face).remote_qabls.remove(res);
        }
    }

    let mut client_qabls = client_qabls(res);
    let router_qabls = remote_router_qabls(tables, res);
    let peer_qabls = remote_peer_qabls(tables, res);

    match tables.whatami {
        WhatAmI::Router => {
            if client_qabls.is_empty() && !peer_qabls {
                undeclare_router_queryable(tables, None, res, &tables.zid.clone());
            } else {
                let local_info = local_router_qabl_info(tables, res);
                register_router_queryable(tables, None, res, &local_info, tables.zid);
                propagate_forget_simple_queryable_to_peers(tables, res);
            }
        }
        WhatAmI::Peer => {
            if tables.hat.full_net(WhatAmI::Peer) {
                if client_qabls.is_empty() {
                    undeclare_peer_queryable(tables, None, res, &tables.zid.clone());
                } else {
                    let local_info = local_peer_qabl_info(tables, res);
                    register_peer_queryable(tables, None, res, &local_info, tables.zid);
                }
            } else if client_qabls.is_empty() {
                propagate_forget_simple_queryable(tables, res);
            } else {
                propagate_simple_queryable(tables, res, None);
            }
        }
        _ => {
            if client_qabls.is_empty() {
                propagate_forget_simple_queryable(tables, res);
            } else {
                propagate_simple_queryable(tables, res, None);
            }
        }
    }

    if client_qabls.len() == 1 && !router_qabls && !peer_qabls {
        let face = &mut client_qabls[0];
        if face.local_qabls.contains_key(res) {
            let wire_expr = Resource::get_best_key(res, "", face.id);
            face.primitives.send_declare(Declare {
                ext_qos: ext::QoSType::declare_default(),
                ext_tstamp: None,
                ext_nodeid: ext::NodeIdType::default(),
                body: DeclareBody::UndeclareQueryable(UndeclareQueryable {
                    id: 0, // TODO
                    ext_wire_expr: WireExprType { wire_expr },
                }),
            });

            get_mut_unchecked(face).local_qabls.remove(res);
        }
    }
}

pub fn forget_client_queryable(
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
                undeclare_client_queryable(&mut wtables, face, &mut res);
                disable_matches_query_routes(&mut wtables, &mut res);
                drop(wtables);

                let rtables = zread!(tables.tables);
                let matches_query_routes = compute_matches_query_routes_(&rtables, &res);
                drop(rtables);

                let wtables = zwrite!(tables.tables);
                for (mut res, query_routes) in matches_query_routes {
                    get_mut_unchecked(&mut res)
                        .context_mut()
                        .update_query_routes(query_routes);
                }
                Resource::clean(&mut res);
                drop(wtables);
            }
            None => log::error!("Undeclare unknown queryable!"),
        },
        None => log::error!("Undeclare queryable with unknown scope!"),
    }
}

pub(crate) fn queries_new_face(tables: &mut Tables, face: &mut Arc<FaceState>) {
    match tables.whatami {
        WhatAmI::Router => {
            if face.whatami == WhatAmI::Client {
                for qabl in tables.hat.router_qabls.iter() {
                    if qabl.context.is_some() {
                        let info = local_qabl_info(tables, qabl, face);
                        get_mut_unchecked(face)
                            .local_qabls
                            .insert(qabl.clone(), info);
                        let key_expr = Resource::decl_key(qabl, face);
                        face.primitives.send_declare(Declare {
                            ext_qos: ext::QoSType::declare_default(),
                            ext_tstamp: None,
                            ext_nodeid: ext::NodeIdType::default(),
                            body: DeclareBody::DeclareQueryable(DeclareQueryable {
                                id: 0, // TODO
                                wire_expr: key_expr,
                                ext_info: info,
                            }),
                        });
                    }
                }
            } else if face.whatami == WhatAmI::Peer && !tables.hat.full_net(WhatAmI::Peer) {
                for qabl in tables.hat.router_qabls.iter() {
                    if qabl.context.is_some()
                        && (qabl.context().router_qabls.keys().any(|r| *r != tables.zid)
                            || qabl.session_ctxs.values().any(|s| {
                                s.qabl.is_some()
                                    && (s.face.whatami == WhatAmI::Client
                                        || (s.face.whatami == WhatAmI::Peer
                                            && tables.hat.failover_brokering(s.face.zid, face.zid)))
                            }))
                    {
                        let info = local_qabl_info(tables, qabl, face);
                        get_mut_unchecked(face)
                            .local_qabls
                            .insert(qabl.clone(), info);
                        let key_expr = Resource::decl_key(qabl, face);
                        face.primitives.send_declare(Declare {
                            ext_qos: ext::QoSType::declare_default(),
                            ext_tstamp: None,
                            ext_nodeid: ext::NodeIdType::default(),
                            body: DeclareBody::DeclareQueryable(DeclareQueryable {
                                id: 0, // TODO
                                wire_expr: key_expr,
                                ext_info: info,
                            }),
                        });
                    }
                }
            }
        }
        WhatAmI::Peer => {
            if tables.hat.full_net(WhatAmI::Peer) {
                if face.whatami == WhatAmI::Client {
                    for qabl in &tables.hat.peer_qabls {
                        if qabl.context.is_some() {
                            let info = local_qabl_info(tables, qabl, face);
                            get_mut_unchecked(face)
                                .local_qabls
                                .insert(qabl.clone(), info);
                            let key_expr = Resource::decl_key(qabl, face);
                            face.primitives.send_declare(Declare {
                                ext_qos: ext::QoSType::declare_default(),
                                ext_tstamp: None,
                                ext_nodeid: ext::NodeIdType::default(),
                                body: DeclareBody::DeclareQueryable(DeclareQueryable {
                                    id: 0, // TODO
                                    wire_expr: key_expr,
                                    ext_info: info,
                                }),
                            });
                        }
                    }
                }
            } else {
                for face in tables
                    .faces
                    .values()
                    .cloned()
                    .collect::<Vec<Arc<FaceState>>>()
                {
                    for qabl in face.remote_qabls.iter() {
                        propagate_simple_queryable(tables, qabl, Some(&mut face.clone()));
                    }
                }
            }
        }
        WhatAmI::Client => {
            for face in tables
                .faces
                .values()
                .cloned()
                .collect::<Vec<Arc<FaceState>>>()
            {
                for qabl in face.remote_qabls.iter() {
                    propagate_simple_queryable(tables, qabl, Some(&mut face.clone()));
                }
            }
        }
    }
}

pub(crate) fn queries_remove_node(tables: &mut Tables, node: &ZenohId, net_type: WhatAmI) {
    match net_type {
        WhatAmI::Router => {
            let mut qabls = vec![];
            for res in tables.hat.router_qabls.iter() {
                for qabl in res.context().router_qabls.keys() {
                    if qabl == node {
                        qabls.push(res.clone());
                    }
                }
            }
            for mut res in qabls {
                unregister_router_queryable(tables, &mut res, node);

                let matches_query_routes = compute_matches_query_routes_(tables, &res);
                for (mut res, query_routes) in matches_query_routes {
                    get_mut_unchecked(&mut res)
                        .context_mut()
                        .update_query_routes(query_routes);
                }
                Resource::clean(&mut res);
            }
        }
        WhatAmI::Peer => {
            let mut qabls = vec![];
            for res in tables.hat.router_qabls.iter() {
                for qabl in res.context().router_qabls.keys() {
                    if qabl == node {
                        qabls.push(res.clone());
                    }
                }
            }
            for mut res in qabls {
                unregister_peer_queryable(tables, &mut res, node);

                if tables.whatami == WhatAmI::Router {
                    let client_qabls = res.session_ctxs.values().any(|ctx| ctx.qabl.is_some());
                    let peer_qabls = remote_peer_qabls(tables, &res);
                    if !client_qabls && !peer_qabls {
                        undeclare_router_queryable(tables, None, &mut res, &tables.zid.clone());
                    } else {
                        let local_info = local_router_qabl_info(tables, &res);
                        register_router_queryable(tables, None, &mut res, &local_info, tables.zid);
                    }
                }

                let matches_query_routes = compute_matches_query_routes_(tables, &res);
                for (mut res, query_routes) in matches_query_routes {
                    get_mut_unchecked(&mut res)
                        .context_mut()
                        .update_query_routes(query_routes);
                }
                Resource::clean(&mut res)
            }
        }
        _ => (),
    }
}

pub(crate) fn queries_linkstate_change(tables: &mut Tables, zid: &ZenohId, links: &[ZenohId]) {
    if let Some(src_face) = tables.get_face(zid) {
        if tables.hat.router_peers_failover_brokering
            && tables.whatami == WhatAmI::Router
            && src_face.whatami == WhatAmI::Peer
        {
            for res in &src_face.remote_qabls {
                let client_qabls = res
                    .session_ctxs
                    .values()
                    .any(|ctx| ctx.face.whatami == WhatAmI::Client && ctx.qabl.is_some());
                if !remote_router_qabls(tables, res) && !client_qabls {
                    for ctx in get_mut_unchecked(&mut res.clone())
                        .session_ctxs
                        .values_mut()
                    {
                        let dst_face = &mut get_mut_unchecked(ctx).face;
                        if dst_face.whatami == WhatAmI::Peer && src_face.zid != dst_face.zid {
                            if dst_face.local_qabls.contains_key(res) {
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
                                                && ctx2.qabl.is_some()
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
                                        body: DeclareBody::UndeclareQueryable(UndeclareQueryable {
                                            id: 0, // TODO
                                            ext_wire_expr: WireExprType { wire_expr },
                                        }),
                                    });

                                    get_mut_unchecked(dst_face).local_qabls.remove(res);
                                }
                            } else if HatTables::failover_brokering_to(links, ctx.face.zid) {
                                let dst_face = &mut get_mut_unchecked(ctx).face;
                                let info = local_qabl_info(tables, res, dst_face);
                                get_mut_unchecked(dst_face)
                                    .local_qabls
                                    .insert(res.clone(), info);
                                let key_expr = Resource::decl_key(res, dst_face);
                                dst_face.primitives.send_declare(Declare {
                                    ext_qos: ext::QoSType::declare_default(),
                                    ext_tstamp: None,
                                    ext_nodeid: ext::NodeIdType::default(),
                                    body: DeclareBody::DeclareQueryable(DeclareQueryable {
                                        id: 0, // TODO
                                        wire_expr: key_expr,
                                        ext_info: info,
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

pub(crate) fn queries_tree_change(
    tables: &mut Tables,
    new_childs: &[Vec<NodeIndex>],
    net_type: WhatAmI,
) {
    // propagate qabls to new childs
    for (tree_sid, tree_childs) in new_childs.iter().enumerate() {
        if !tree_childs.is_empty() {
            let net = tables.hat.get_net(net_type).unwrap();
            let tree_idx = NodeIndex::new(tree_sid);
            if net.graph.contains_node(tree_idx) {
                let tree_id = net.graph[tree_idx].zid;

                let qabls_res = match net_type {
                    WhatAmI::Router => &tables.hat.router_qabls,
                    _ => &tables.hat.peer_qabls,
                };

                for res in qabls_res {
                    let qabls = match net_type {
                        WhatAmI::Router => &res.context().router_qabls,
                        _ => &res.context().peer_qabls,
                    };
                    if let Some(qabl_info) = qabls.get(&tree_id) {
                        send_sourced_queryable_to_net_childs(
                            tables,
                            net,
                            tree_childs,
                            res,
                            qabl_info,
                            None,
                            Some(tree_sid as u16),
                        );
                    }
                }
            }
        }
    }

    // recompute routes
    compute_query_routes_from(tables, &mut tables.root_res.clone());
}