#include <gtk/gtk.h>
#include <stdio.h>
static GtkWidget *entry;
static GtkWidget *status;
static void changed(GtkEditable *editable, void *data) {
    (void)data; printf("TEXT:%s\n", gtk_editable_get_text(editable)); fflush(stdout);
}
static void copied(GtkButton *button, void *data) {
    (void)button; (void)data;
    gdk_clipboard_set_text(gtk_widget_get_clipboard(entry),gtk_editable_get_text(GTK_EDITABLE(entry)));
    gtk_label_set_text(GTK_LABEL(status),"Copied entry text");
    puts("COPY");fflush(stdout);
}
static void pasted(GObject *clipboard, GAsyncResult *result, void *data) {
    (void)data;GError *error=NULL;
    char *text=gdk_clipboard_read_text_finish(GDK_CLIPBOARD(clipboard),result,&error);
    if(error) {gtk_label_set_text(GTK_LABEL(status),error->message);g_error_free(error);return;}
    if(text) {gtk_editable_set_text(GTK_EDITABLE(entry),text);g_free(text);}
    gtk_label_set_text(GTK_LABEL(status),"Pasted clipboard text");
}
static void paste(GtkButton *button, void *data) {
    (void)button;(void)data;
    gdk_clipboard_read_text_async(gtk_widget_get_clipboard(entry),NULL,pasted,NULL);
}
static void motion(GtkEventControllerMotion *controller, double x, double y, void *data) {
    (void)controller;(void)data;
    printf("MOTION:%.3f,%.3f\n",x,y);fflush(stdout);
}
int main(int argc, char **argv) {
    gtk_init();
    GtkWidget *window=gtk_window_new();
    gtk_window_set_title(GTK_WINDOW(window),argc>1?argv[1]:"Clipboard fixture");
    gtk_window_set_default_size(GTK_WINDOW(window),480,220);
    GtkWidget *box=gtk_box_new(GTK_ORIENTATION_VERTICAL,16);
    gtk_widget_set_margin_start(box,16);gtk_widget_set_margin_end(box,16);
    gtk_widget_set_margin_top(box,16);gtk_widget_set_margin_bottom(box,16);
    entry=gtk_entry_new();
    gtk_entry_set_placeholder_text(GTK_ENTRY(entry),"Clipboard test input");
    if(argc>2)gtk_editable_set_text(GTK_EDITABLE(entry),argv[2]);
    gtk_box_append(GTK_BOX(box),entry);
    GtkWidget *buttons=gtk_box_new(GTK_ORIENTATION_HORIZONTAL,16);
    GtkWidget *copy=gtk_button_new_with_label("Copy");GtkWidget *paste_button=gtk_button_new_with_label("Paste");
    gtk_widget_set_size_request(copy,120,40);gtk_widget_set_size_request(paste_button,120,40);
    g_signal_connect(copy,"clicked",G_CALLBACK(copied),NULL);g_signal_connect(paste_button,"clicked",G_CALLBACK(paste),NULL);
    gtk_box_append(GTK_BOX(buttons),copy);gtk_box_append(GTK_BOX(buttons),paste_button);gtk_box_append(GTK_BOX(box),buttons);
    status=gtk_label_new("Move the mouse here to record human input");gtk_box_append(GTK_BOX(box),status);
    gtk_window_set_child(GTK_WINDOW(window),box);
    g_signal_connect(entry,"changed",G_CALLBACK(changed),NULL);
    GtkEventController *pointer=gtk_event_controller_motion_new();
    g_signal_connect(pointer,"motion",G_CALLBACK(motion),NULL);gtk_widget_add_controller(box,pointer);
    gtk_window_present(GTK_WINDOW(window));gtk_widget_grab_focus(entry);
    GMainLoop *loop=g_main_loop_new(NULL,FALSE);g_main_loop_run(loop);return 0;
}
