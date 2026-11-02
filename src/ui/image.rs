//! Aspect-preserving images measured against the space available to the row.
//!
//! GPUI's image element resolves both intrinsic dimensions before applying
//! max-width/height. Measure the fitted size together so neither the hit area
//! nor a surrounding border includes letterboxing. Asset loading and decoding
//! still belong to GPUI's background asset cache.

use std::sync::Arc;

use gpui::{
    AnyElement, App, AvailableSpace, Bounds, Element, ElementId, GlobalElementId, ImageSource,
    InspectorElementId, IntoElement, LayoutId, ObjectFit, Pixels, RenderImage, Size, Window, div,
    img, prelude::*, px, size,
};

pub const TRANSCRIPT_IMAGE_MAX_WIDTH: f32 = 640.0;
pub const TRANSCRIPT_IMAGE_MAX_HEIGHT: f32 = 320.0;

pub struct FittedImage {
    source: ImageSource,
    max_size: Size<Pixels>,
    radius: Pixels,
}

pub fn fitted_image(
    source: impl Into<ImageSource>,
    max_size: Size<Pixels>,
    radius: Pixels,
) -> FittedImage {
    FittedImage {
        source: source.into(),
        max_size,
        radius,
    }
}

impl Element for FittedImage {
    type RequestLayoutState = Option<Arc<RenderImage>>;
    type PrepaintState = AnyElement;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let image = match &self.source {
            ImageSource::Image(image) => image.clone().use_render_image(window, cx),
            ImageSource::Render(image) => Some(image.clone()),
            ImageSource::Resource(resource) => window
                .use_asset::<gpui::ImgResourceLoader>(resource, cx)
                .and_then(Result::ok),
            ImageSource::Custom(load) => load(window, cx).and_then(Result::ok),
        };
        let intrinsic = image.as_ref().map_or(self.max_size, |image| {
            image.size(0).map(|dimension| px(dimension.0 as f32))
        });
        let max_size = self.max_size;
        let layout = window.request_measured_layout(
            gpui::Style {
                aspect_ratio: Some(intrinsic.width / intrinsic.height),
                ..Default::default()
            },
            move |known, available, _, _| {
                let width = known.width.or(match available.width {
                    AvailableSpace::Definite(width) => Some(width),
                    _ => None,
                });
                let height = known.height.or(match available.height {
                    AvailableSpace::Definite(height) => Some(height),
                    _ => None,
                });
                let scale = 1.0_f32
                    .min(width.unwrap_or(max_size.width).min(max_size.width) / intrinsic.width)
                    .min(height.unwrap_or(max_size.height).min(max_size.height) / intrinsic.height)
                    .max(0.0);
                size(intrinsic.width * scale, intrinsic.height * scale)
            },
        );
        (layout, image)
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        image: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let mut element = if let Some(image) = image {
            img(image.clone())
                .id("fitted-image")
                .w(bounds.size.width)
                .h(bounds.size.height)
                .rounded(self.radius)
                .object_fit(ObjectFit::Contain)
                .into_any_element()
        } else {
            div()
                .w(bounds.size.width)
                .h(bounds.size.height)
                .into_any_element()
        };
        element.layout_as_root(bounds.size.map(AvailableSpace::Definite), window, cx);
        element.prepaint_at(bounds.origin, window, cx);
        element
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        element: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        element.paint(window, cx);
    }
}

impl IntoElement for FittedImage {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, rc::Rc};

    #[gpui::test]
    fn fitted_image_layout_preserves_the_ratio_inside_its_border(cx: &mut gpui::TestAppContext) {
        struct Harness {
            image: Arc<RenderImage>,
            width: Pixels,
            bounds: Rc<Cell<Bounds<Pixels>>>,
        }

        impl gpui::Render for Harness {
            fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
                let bounds = self.bounds.clone();
                div().w(self.width).flex().flex_col().items_start().child(
                    div()
                        .relative()
                        .max_w(gpui::relative(1.0))
                        .border_1()
                        .flex()
                        .flex_col()
                        .items_start()
                        .child(fitted_image(
                            self.image.clone(),
                            size(px(300.0), px(200.0)),
                            px(5.0),
                        ))
                        .child(
                            gpui::canvas(move |value, _, _| bounds.set(value), |_, _, _, _| ())
                                .absolute()
                                .inset_0(),
                        ),
                )
            }
        }

        for (width, height, viewport) in [
            (400, 200, 600.0),
            (200, 400, 600.0),
            (400, 400, 600.0),
            (400, 200, 160.0),
        ] {
            let image = Arc::new(RenderImage::new([image::Frame::new(
                image::RgbaImage::new(width, height),
            )]));
            let bounds = Rc::new(Cell::new(Bounds::default()));
            let (_, window) = cx.add_window_view(|_, _| Harness {
                image,
                width: px(viewport),
                bounds: bounds.clone(),
            });
            window.run_until_parked();
            let measured = bounds.get().size;
            // The probe is absolutely positioned inside the one-pixel border.
            let ratio = f32::from(measured.width) / f32::from(measured.height);
            assert!(
                (ratio - width as f32 / height as f32).abs() < 0.02,
                "{width}x{height} in {viewport}: {measured:?}"
            );
            assert!(measured.width <= px(viewport));
            assert!(measured.width <= px(300.0));
            assert!(measured.height <= px(200.0));
            assert!(
                measured.width >= px(100.0),
                "the image should use the available space"
            );
        }
    }
}
