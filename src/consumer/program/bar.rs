use crate::consumer::program::Runner;

use iced::{
    Alignment, Border, Center, Font, Length, Padding, Shadow, Theme,
    widget::{self, button, container, mouse_area, row, svg, text},
};

use super::*;

impl Runner {
    pub fn bar(&self) -> Element<'_> {
        let left = widget::row![
            self.logo().into(),
            self.workspace().into(),
            self.title().into()
        ]
        .align_y(Center)
        .spacing(7)
        .padding(Padding::new(0.0).left(16))
        .width(Length::Fill)
        .height(Length::Fill);

        let right = widget::row![
            self.tray(),
            self.volume(),
            self.backlight(),
            self.battery(),
            self.clock().into()
        ]
        .align_y(Center)
        .padding(Padding::new(0.0).right(13))
        .spacing(9)
        .height(Length::Fill);

        widget::row![left, right].into()
    }
    fn logo(&self) -> impl Into<Element<'_>> {
        button(
            svg("/usr/share/pixmaps/archlinux-logo.svg")
                .style(|theme: &Theme, status| svg::Style {
                    color: Some(match status {
                        svg::Status::Idle => theme.palette().text,
                        svg::Status::Hovered => theme.palette().primary,
                    }),
                })
                .width(23),
        )
        .style(|_, _| button::Style::default())
        .on_press(Message::Hello)
        .padding(0)
        .clip(false)
    }
    fn workspace_item(&self, idx: usize) -> Element<'_> {
        let id = (idx + 1) as _;
        let alive = self.workspaces.get(idx);
        let urgent = self.urgent_workspaces.get(idx);
        let focused = idx == self.workspace_focused;
        let text: Element = if focused {
            text(id % 10).size(11.5).shaping(Shaping::Basic).into()
        } else {
            match alive {
                true => text(id % 10).size(11.5),
                false => text("𒊹")
                    .size(4.5)
                    .font(Font::with_name("Noto Sans Cuneiform")),
            }
            .into()
        };
        let container = container(text).center_x(15).center_y(16);
        let button = button(container)
            .style(move |theme: &Theme, status| button::Style {
                background: match (status, focused) {
                    (button::Status::Hovered, true) => Some(theme.palette().primary.into()),
                    (button::Status::Hovered, false) => None,
                    (_, true) => Some(theme.palette().text.into()),
                    (_, false) => urgent.then_some(theme.palette().danger.into()),
                },
                text_color: match focused || urgent {
                    true => theme.palette().background.with_alpha(1.0),
                    false => match status {
                        button::Status::Hovered => theme.palette().primary,
                        _ => theme.palette().text,
                    },
                },
                border: Border::default().rounded(3),
                shadow: match status {
                    button::Status::Hovered => Shadow {
                        color: theme.palette().primary.with_alpha(if focused {
                            0.55
                        } else {
                            0.34
                        }),
                        blur_radius: 10.0,
                        ..Default::default()
                    },
                    _ => Default::default(),
                },
                ..Default::default()
            })
            .padding(0)
            .on_press(Message::Workspace { id });
        button.into()
    }
    fn workspace(&self) -> impl Into<Element<'_>> {
        row((0..WORKSPACE_MAX).map(|idx| self.workspace_item(idx)))
            .spacing(2)
            .align_y(Center)
    }
    fn title(&self) -> impl Into<Element<'_>> {
        let icon = self.window.icon.as_ref().map(Handle::load);
        let class = text(self.window.class.as_str())
            .style(|theme: &Theme| text::Style {
                color: Some(theme.palette().primary),
            })
            .size(14.5)
            .shaping(Shaping::Basic);
        let title = text(self.window.title.as_str());
        let row = row([icon.into(), class.into(), title.into()])
            .align_y(Center)
            .spacing(5);

        mouse_area(row)
            // .on_enter(Signal::Message(Message::Hello))
            // .on_exit(Signal::Message(Message::Bye))
            .on_enter(Message::WindowInfo)
            .on_exit(Message::CloseTooltip)
    }
    fn tray(&self) -> Element<'_> {
        row(self
            .tray_items
            .iter()
            .filter_map(|(service, TrayItem { icon })| {
                icon.as_ref().map(|icon| {
                    mouse_area(icon.load_size(22))
                        .on_enter(Message::TrayTooltip(service.clone()))
                        .on_exit(Message::CloseTooltip)
                        .on_press(Message::TrayAction(service.clone()))
                        .into()
                })
            }))
        .spacing(7)
        .into()
    }
    fn volume(&self) -> Option<Element<'_>> {
        Some(
            mouse_area(self.volume.icon.as_ref()?.load_size(17.5))
                .on_enter(Message::Volume)
                .on_exit(Message::CloseTooltip)
                .into(),
        )
    }
    fn backlight(&self) -> Option<Element<'_>> {
        Some(
            mouse_area(self.backlight.as_ref()?.icon.as_ref()?.load_size(17.5))
                .on_enter(Message::Backlight)
                .on_exit(Message::CloseTooltip)
                .into(),
        )
    }
    fn battery(&self) -> Option<Element<'_>> {
        Some(
            mouse_area(self.battery_status.as_ref()?.icon.as_ref()?.load_size(17.5))
                .on_enter(Message::Battery)
                .on_exit(Message::BatteryStop)
                .into(),
        )
    }
    fn clock(&self) -> impl Into<Element<'_>> {
        let date = text(unsafe { str::from_utf8_unchecked(&self.date) })
            .size(12.5)
            .height(Length::Fill)
            .align_y(Alignment::End);
        let date = container(date)
            .padding(Padding::default().bottom(7.5))
            .into();
        let time = text(unsafe { str::from_utf8_unchecked(&self.time) })
            .size(17)
            .height(Length::Fill)
            .shaping(Shaping::Basic)
            .center()
            .width(64)
            .align_x(Center)
            .into();
        let weekday = text(self.weekday).size(15).height(Length::Fill).center();
        let weekday = container(weekday)
            .padding(Padding::default().bottom(4.5))
            .into();
        row([date, time, weekday]).spacing(7)
    }
}
