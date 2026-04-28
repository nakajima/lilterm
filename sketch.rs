pub trait TranscriptEntry {
    type V: View;
    fn id(&self) -> String;
    fn view(&self) -> V;
}

session.update(|ui| {
    for message in self.newly_completed_messages {
        ui.insert_scrollback(message.view());
    }

    for message in self.updating_messages {
        ui.render_live(message.view());
    }

    ui.render_footer(|footer| {
        if self.is_working() {
           footer.render(WorkingIndicator());
       }

        footer.render(self.editor);

        if let Some(picker) = self.current_picker {
            footer.render(picker);
        }
    })
});
