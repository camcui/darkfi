/* This file is part of DarkFi (https://dark.fi)
 *
 * Copyright (C) 2020-2024 Dyne.org foundation
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU Affero General Public License as
 * published by the Free Software Foundation, either version 3 of the
 * License, or (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU Affero General Public License for more details.
 *
 * You should have received a copy of the GNU Affero General Public License
 * along with this program.  If not, see <https://www.gnu.org/licenses/>.
 */

use async_trait::async_trait;
use darkfi_serial::Encodable;
use image::ImageReader;
use miniquad::{MouseButton, TouchPhase};
use rand::{rngs::OsRng, Rng};
use std::{
    io::Cursor,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex as SyncMutex, OnceLock, Weak,
    },
};

use crate::{
    gfx::{
        GfxDrawCall, GfxDrawInstruction, GfxDrawMesh, GfxTextureId, ManagedTexturePtr, Point,
        Rectangle, RenderApi,
    },
    mesh::{MeshBuilder, MeshInfo, COLOR_WHITE},
    prop::{PropertyFloat32, PropertyPtr, PropertyRect, PropertyStr, PropertyUint32, Role},
    scene::{Pimpl, SceneNodePtr, SceneNodeWeak},
    text::{self, GlyphPositionIter, TextShaper, TextShaperPtr},
    util::unixtime,
    ExecutorPtr,
};

use super::{DrawUpdate, OnModify, UIObject};

mod emoji;

macro_rules! d { ($($arg:tt)*) => { debug!(target: "ui::emoji_picker", $($arg)*); } }

pub type EmojiMeshesPtr = Arc<SyncMutex<EmojiMeshes>>;

pub struct EmojiMeshes {
    render_api: RenderApi,
    text_shaper: TextShaperPtr,
    emoji_size: f32,
    meshes: Vec<GfxDrawMesh>,
}

impl EmojiMeshes {
    pub fn new(
        render_api: RenderApi,
        text_shaper: TextShaperPtr,
        emoji_size: f32,
    ) -> EmojiMeshesPtr {
        Arc::new(SyncMutex::new(Self {
            render_api,
            text_shaper,
            emoji_size,
            meshes: Vec::with_capacity(emoji::EMOJI_LIST.len()),
        }))
    }

    /// Make mesh for this emoji centered at (0, 0)
    fn gen_emoji_mesh(&self, emoji: &str) -> GfxDrawMesh {
        //d!("rendering emoji: '{emoji}'");
        // The params here don't actually matter since we're talking about BMP fixed sizes
        let glyphs = self.text_shaper.shape(emoji.to_string(), 10., 1.);
        assert_eq!(glyphs.len(), 1);
        let atlas = text::make_texture_atlas(&self.render_api, &glyphs);
        let glyph = glyphs.into_iter().next().unwrap();

        // Emoji's vary in size. We make them all a consistent size.
        let w = self.emoji_size;
        let h =
            (glyph.sprite.bmp_height as f32) * self.emoji_size / (glyph.sprite.bmp_width as f32);
        // Center at origin
        let x = -w / 2.;
        let y = -h / 2.;

        let uv = atlas.fetch_uv(glyph.glyph_id).expect("missing glyph UV rect");
        let mut mesh = MeshBuilder::new();
        mesh.draw_box(&Rectangle::new(x, y, w, h), COLOR_WHITE, &uv);
        mesh.alloc(&self.render_api).draw_with_texture(atlas.texture)
    }

    pub fn get(&mut self, i: usize) -> GfxDrawMesh {
        assert!(i < emoji::EMOJI_LIST.len());

        if i >= self.meshes.len() {
            //d!("EmojiMeshes loading new glyphs");
            for j in self.meshes.len()..=i {
                let emoji = emoji::EMOJI_LIST[j];
                let mesh = self.gen_emoji_mesh(emoji);
                self.meshes.push(mesh);
            }
        }

        self.meshes[i].clone()
    }
}

struct TouchInfo {
    start_pos: Point,
    start_scroll: f32,
    is_scroll: bool,
}

pub type EmojiPickerPtr = Arc<EmojiPicker>;

pub struct EmojiPicker {
    node: SceneNodeWeak,
    render_api: RenderApi,
    tasks: OnceLock<Vec<smol::Task<()>>>,

    dc_key: u64,
    emoji_meshes: EmojiMeshesPtr,

    rect: PropertyRect,
    z_index: PropertyUint32,
    priority: PropertyUint32,
    scroll: PropertyFloat32,
    emoji_size: PropertyFloat32,
    mouse_scroll_speed: PropertyFloat32,

    window_scale: PropertyFloat32,
    parent_rect: SyncMutex<Option<Rectangle>>,
    is_mouse_hover: AtomicBool,
    touch_info: SyncMutex<Option<TouchInfo>>,
}

impl EmojiPicker {
    pub async fn new(
        node: SceneNodeWeak,
        window_scale: PropertyFloat32,
        render_api: RenderApi,
        emoji_meshes: EmojiMeshesPtr,
        ex: ExecutorPtr,
    ) -> Pimpl {
        d!("EmojiPicker::new()");

        let node_ref = &node.upgrade().unwrap();
        let rect = PropertyRect::wrap(node_ref, Role::Internal, "rect").unwrap();
        let z_index = PropertyUint32::wrap(node_ref, Role::Internal, "z_index", 0).unwrap();
        let priority = PropertyUint32::wrap(node_ref, Role::Internal, "priority", 0).unwrap();
        let scroll = PropertyFloat32::wrap(node_ref, Role::Internal, "scroll", 0).unwrap();
        let emoji_size = PropertyFloat32::wrap(node_ref, Role::Internal, "emoji_size", 0).unwrap();
        let mouse_scroll_speed =
            PropertyFloat32::wrap(node_ref, Role::Internal, "mouse_scroll_speed", 0).unwrap();

        let node_name = node_ref.name.clone();
        let node_id = node_ref.id;

        let self_ = Arc::new(Self {
            node,
            render_api,
            tasks: OnceLock::new(),

            dc_key: OsRng.gen(),
            emoji_meshes,

            rect,
            z_index,
            priority,
            scroll,
            emoji_size,
            mouse_scroll_speed,

            window_scale,
            parent_rect: SyncMutex::new(None),
            is_mouse_hover: AtomicBool::new(false),
            touch_info: SyncMutex::new(None),
        });

        Pimpl::EmojiPicker(self_)
    }

    fn emojis_per_line(&self) -> f32 {
        let emoji_size = self.emoji_size.get();
        let rect_w = self.rect.get().w;
        //d!("rect_w = {rect_w}");
        (rect_w / emoji_size).floor()
    }
    fn calc_off_x(&self) -> f32 {
        let emoji_size = self.emoji_size.get();
        let rect_w = self.rect.get().w;
        let n = self.emojis_per_line();
        let off_x = (rect_w - emoji_size) / (n - 1.);
        off_x
    }

    fn max_scroll(&self) -> f32 {
        let emojis_len = emoji::EMOJI_LIST.len() as f32;
        let emoji_size = self.emoji_size.get();
        let cols = self.emojis_per_line();
        let rows = (emojis_len / cols).floor();

        let rect_h = self.rect.get().h;
        let height = rows * emoji_size;
        if height < rect_h {
            return 0.
        }
        height - rect_h
    }

    async fn click_emoji(&self, pos: Point) {
        let n_cols = self.emojis_per_line();
        let emoji_size = self.emoji_size.get();
        let scroll = self.scroll.get();

        // Emojis have spacing along the x axis.
        // If the screen width is 2000, and emoji_size is 30, then that's 66 emojis.
        // But that's 66.66px per emoji.
        let real_width = self.rect.get().w / n_cols;
        //d!("click_emoji({pos:?})");
        let col = (pos.x / real_width).floor();

        let y = pos.y + scroll;
        let row = (y / emoji_size).floor();
        //d!("emoji_size = {emoji_size}, col = {col}, row = {row}");

        //d!("idx = col + row * n_cols = {col} + {row} * {n_cols}");
        let idx = (col + row * n_cols).round() as usize;
        //d!("    = {idx}, emoji_len = {}", emoji::EMOJI_LIST.len());

        if idx < emoji::EMOJI_LIST.len() {
            let emoji = emoji::EMOJI_LIST[idx];
            d!("Selected emoji: {emoji}");
            let mut param_data = vec![];
            emoji.encode(&mut param_data).unwrap();
            let node = self.node.upgrade().unwrap();
            node.trigger("emoji_select", param_data).await.unwrap();
        } else {
            d!("Index out of bounds");
        }
    }

    fn redraw(&self) {
        let timest = unixtime();
        let Some(parent_rect) = self.parent_rect.lock().unwrap().clone() else { return };

        let Some(draw_update) = self.get_draw_calls(parent_rect) else {
            error!(target: "ui::emoji_picker", "Emoji picker failed to draw");
            return;
        };
        self.render_api.replace_draw_calls(timest, draw_update.draw_calls);
        d!("replace draw calls done");
    }

    fn get_draw_calls(&self, parent_rect: Rectangle) -> Option<DrawUpdate> {
        if let Err(e) = self.rect.eval(&parent_rect) {
            warn!(target: "ui::emoji_picker", "Rect eval failed: {e}");
            return None
        }

        // Clamp scroll if needed due to window size change
        let max_scroll = self.max_scroll();
        if self.scroll.get() > max_scroll {
            self.scroll.set(max_scroll);
        }

        let rect = self.rect.get();
        let mut instrs = vec![GfxDrawInstruction::ApplyView(rect)];

        let off_x = self.calc_off_x();
        let emoji_size = self.emoji_size.get();

        let mut emoji_meshes = self.emoji_meshes.lock().unwrap();

        let mut x = emoji_size / 2.;
        let mut y = emoji_size / 2. - self.scroll.get();
        for (i, mesh) in emoji::EMOJI_LIST.iter().enumerate() {
            let pos = Point::new(x, y);
            let mesh = emoji_meshes.get(i);
            instrs.extend_from_slice(&[
                GfxDrawInstruction::Move(pos),
                GfxDrawInstruction::Draw(mesh),
            ]);

            x += off_x;
            if x > rect.w {
                x = emoji_size / 2.;
                y += emoji_size;
                //d!("Line break after idx={i}");
            }

            if y > rect.h + emoji_size {
                break
            }
        }

        Some(DrawUpdate {
            key: self.dc_key,
            draw_calls: vec![(
                self.dc_key,
                GfxDrawCall { instrs, dcs: vec![], z_index: self.z_index.get() },
            )],
        })
    }
}

#[async_trait]
impl UIObject for EmojiPicker {
    fn priority(&self) -> u32 {
        self.priority.get()
    }

    async fn start(self: Arc<Self>, ex: ExecutorPtr) {
        let me = Arc::downgrade(&self);

        let node_ref = &self.node.upgrade().unwrap();
        let node_name = node_ref.name.clone();
        let node_id = node_ref.id;

        async fn redraw(self_: Arc<EmojiPicker>) {
            self_.redraw();
        }

        let mut on_modify = OnModify::new(ex, node_name, node_id, me.clone());
        on_modify.when_change(self.rect.prop(), redraw);
        on_modify.when_change(self.z_index.prop(), redraw);

        self.tasks.set(on_modify.tasks);
    }

    async fn draw(&self, parent_rect: Rectangle) -> Option<DrawUpdate> {
        d!("EmojiPicker::draw()");
        *self.parent_rect.lock().unwrap() = Some(parent_rect);
        self.get_draw_calls(parent_rect)
    }

    async fn handle_mouse_move(&self, mut mouse_pos: Point) -> bool {
        let rect = self.rect.get();
        self.is_mouse_hover.store(rect.contains(mouse_pos), Ordering::Relaxed);
        false
    }

    async fn handle_mouse_wheel(&self, wheel_pos: Point) -> bool {
        if !self.is_mouse_hover.load(Ordering::Relaxed) {
            return false
        }
        d!("handle_mouse_wheel()");

        let mut scroll = self.scroll.get();
        scroll -= self.mouse_scroll_speed.get() * wheel_pos.y;
        scroll = scroll.clamp(0., self.max_scroll());
        self.scroll.set(scroll);

        self.redraw();

        true
    }

    async fn handle_mouse_btn_up(&self, btn: MouseButton, mut mouse_pos: Point) -> bool {
        let rect = self.rect.get();
        if !rect.contains(mouse_pos) {
            return false
        }
        mouse_pos.x -= rect.x;
        mouse_pos.y -= rect.y;
        self.click_emoji(mouse_pos).await;

        true
    }

    async fn handle_touch(&self, phase: TouchPhase, id: u64, mut touch_pos: Point) -> bool {
        // Ignore multi-touch
        if id != 0 {
            return false
        }

        let rect = self.rect.get();
        let pos = touch_pos - Point::new(rect.x, rect.y);

        // We need this cos you cannot hold mutex and call async fn
        // todo: clean this up
        let mut emoji_is_clicked = false;
        {
            let mut touch_info = self.touch_info.lock().unwrap();
            match phase {
                TouchPhase::Started => {
                    if !rect.contains(touch_pos) {
                        return false
                    }

                    *touch_info = Some(TouchInfo {
                        start_pos: pos,
                        start_scroll: self.scroll.get(),
                        is_scroll: false,
                    });
                }
                TouchPhase::Moved => {
                    if let Some(touch_info) = touch_info.as_mut() {
                        let y_diff = touch_info.start_pos.y - pos.y;
                        if y_diff.abs() > 0.5 {
                            touch_info.is_scroll = true;
                        }

                        if touch_info.is_scroll {
                            let mut scroll = touch_info.start_scroll + y_diff;
                            scroll = scroll.clamp(0., self.max_scroll());
                            self.scroll.set(scroll);
                            self.redraw();
                        }
                    } else {
                        return false
                    }
                }
                TouchPhase::Ended | TouchPhase::Cancelled => {
                    if let Some(touch_info) = &*touch_info {
                        if !touch_info.is_scroll {
                            emoji_is_clicked = true;
                        }
                    } else {
                        return false
                    }
                    *touch_info = None;
                }
            }
        }
        if emoji_is_clicked {
            self.click_emoji(pos).await;
        }

        true
    }
}

impl Drop for EmojiPicker {
    fn drop(&mut self) {
        self.render_api.replace_draw_calls(unixtime(), vec![(self.dc_key, Default::default())]);
    }
}
