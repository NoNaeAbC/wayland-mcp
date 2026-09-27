/* Native, visible toolkit client. Extension code is generated from /usr/share
 * into the temporary build directory by demonstrate-input.py. */
#include <gtk/gtk.h>
#include <gdk/wayland/gdkwayland.h>
#include <wayland-client.h>
#include <sys/mman.h>
#include <unistd.h>
#include <stdio.h>
#include <string.h>
#include "relative-pointer-client.h"
#include "pointer-constraints-client.h"
static struct wl_display *display;
static struct wl_compositor *compositor;
static struct wl_subcompositor *subcompositor;
static struct wl_shm *shm;
static struct wl_seat *seat;
static struct wl_pointer *pointer;
static struct wl_touch *touch;
static struct zwp_relative_pointer_manager_v1 *relative_manager;
static struct zwp_pointer_constraints_v1 *constraints;
static struct zwp_relative_pointer_v1 *relative;
static struct zwp_locked_pointer_v1 *locked;
static struct zwp_confined_pointer_v1 *confined;
static struct wl_surface *surface,*child;
static GtkWidget *status;
static void report(const char *message) {puts(message);fflush(stdout);gtk_label_set_text(GTK_LABEL(status),message);}
static void relative_motion(void *data,struct zwp_relative_pointer_v1 *p,uint32_t hi,uint32_t lo,wl_fixed_t dx,wl_fixed_t dy,wl_fixed_t ux,wl_fixed_t uy) {
    (void)data;(void)p;char text[192];snprintf(text,sizeof text,"RELATIVE:%u:%u:%d:%d:%d:%d",hi,lo,dx,dy,ux,uy);report(text);
}
static const struct zwp_relative_pointer_v1_listener relative_listener={relative_motion};
static void lock_on(void *d,struct zwp_locked_pointer_v1 *p){(void)d;(void)p;report("LOCKED");}
static void lock_off(void *d,struct zwp_locked_pointer_v1 *p){(void)d;(void)p;report("UNLOCKED");}
static const struct zwp_locked_pointer_v1_listener lock_listener={lock_on,lock_off};
static void confine_on(void *d,struct zwp_confined_pointer_v1 *p){(void)d;(void)p;report("CONFINED");}
static void confine_off(void *d,struct zwp_confined_pointer_v1 *p){(void)d;(void)p;report("UNCONFINED");}
static const struct zwp_confined_pointer_v1_listener confine_listener={confine_on,confine_off};
static gboolean release_capture(void *unused) {
    (void)unused;if(locked){zwp_locked_pointer_v1_destroy(locked);locked=NULL;}
    if(confined){zwp_confined_pointer_v1_destroy(confined);confined=NULL;}
    wl_display_flush(display);return G_SOURCE_REMOVE;
}
static void capture(GtkButton *button,void *mode) {
    (void)button;release_capture(NULL);
    if(!constraints){report("NO POINTER CONSTRAINTS GLOBAL");return;}
    if(mode){confined=zwp_pointer_constraints_v1_confine_pointer(constraints,surface,pointer,NULL,2);zwp_confined_pointer_v1_add_listener(confined,&confine_listener,NULL);}
    else{locked=zwp_pointer_constraints_v1_lock_pointer(constraints,surface,pointer,NULL,2);zwp_locked_pointer_v1_add_listener(locked,&lock_listener,NULL);}
    wl_surface_commit(surface);wl_display_flush(display);g_timeout_add_seconds(5,release_capture,NULL);
}
static void td(void*d,struct wl_touch*t,uint32_t serial,uint32_t time,struct wl_surface*s,int32_t id,wl_fixed_t x,wl_fixed_t y){(void)d;(void)t;(void)serial;(void)time;(void)s;char b[128];snprintf(b,sizeof b,"TOUCH_DOWN:%d:%d:%d",id,x,y);report(b);}
static void tu(void*d,struct wl_touch*t,uint32_t serial,uint32_t time,int32_t id){(void)d;(void)t;(void)serial;(void)time;char b[64];snprintf(b,sizeof b,"TOUCH_UP:%d",id);report(b);}
static void tm(void*d,struct wl_touch*t,uint32_t time,int32_t id,wl_fixed_t x,wl_fixed_t y){(void)d;(void)t;(void)time;char b[128];snprintf(b,sizeof b,"TOUCH_MOTION:%d:%d:%d",id,x,y);report(b);}
static void tf(void*d,struct wl_touch*t){(void)d;(void)t;puts("TOUCH_FRAME");fflush(stdout);}
static void tc(void*d,struct wl_touch*t){(void)d;(void)t;report("TOUCH_CANCEL");}
static void ts(void*d,struct wl_touch*t,int32_t id,wl_fixed_t major,wl_fixed_t minor){(void)d;(void)t;printf("TOUCH_SHAPE:%d:%d:%d\n",id,major,minor);fflush(stdout);}
static void to(void*d,struct wl_touch*t,int32_t id,wl_fixed_t angle){(void)d;(void)t;printf("TOUCH_ORIENTATION:%d:%d\n",id,angle);fflush(stdout);}
static const struct wl_touch_listener touch_listener={td,tu,tm,tf,tc,ts,to};
static gboolean legacy(GtkEventControllerLegacy *controller,GdkEvent *event,void *unused){(void)controller;(void)unused;GdkEventType type=gdk_event_get_event_type(event);if(type>=GDK_TOUCH_BEGIN&&type<=GDK_TOUCH_CANCEL){printf("GTK_TOUCH:%d\n",type);fflush(stdout);}return FALSE;}
static void global(void*d,struct wl_registry*r,uint32_t name,const char*i,uint32_t v){(void)d;
#define BIND(name_,type_,max_) if(!strcmp(i,name_)){type_=wl_registry_bind(r,name,&type_##_interface,v<max_?v:max_);return;}
    if(!strcmp(i,"wl_compositor")){compositor=wl_registry_bind(r,name,&wl_compositor_interface,v<4?v:4);return;}
    if(!strcmp(i,"wl_subcompositor")){subcompositor=wl_registry_bind(r,name,&wl_subcompositor_interface,1);return;}
    if(!strcmp(i,"wl_shm")){shm=wl_registry_bind(r,name,&wl_shm_interface,1);return;}
    if(!strcmp(i,"wl_seat")){seat=wl_registry_bind(r,name,&wl_seat_interface,v<7?v:7);return;}
    if(!strcmp(i,"zwp_relative_pointer_manager_v1")){relative_manager=wl_registry_bind(r,name,&zwp_relative_pointer_manager_v1_interface,1);return;}
    if(!strcmp(i,"zwp_pointer_constraints_v1")){constraints=wl_registry_bind(r,name,&zwp_pointer_constraints_v1_interface,1);return;}
}
static void removed(void*d,struct wl_registry*r,uint32_t n){(void)d;(void)r;(void)n;}
static const struct wl_registry_listener registry_listener={global,removed};
static gboolean native_setup(void *window) {
    GdkSurface *gdk=gtk_native_get_surface(GTK_NATIVE(window));if(!gdk)return G_SOURCE_CONTINUE;
    surface=gdk_wayland_surface_get_wl_surface(gdk);
    display=gdk_wayland_display_get_wl_display(gdk_surface_get_display(gdk));
    struct wl_registry *registry=wl_display_get_registry(display);wl_registry_add_listener(registry,&registry_listener,NULL);wl_display_roundtrip(display);
    pointer=wl_seat_get_pointer(seat);touch=wl_seat_get_touch(seat);wl_touch_add_listener(touch,&touch_listener,NULL);
    if(relative_manager){relative=zwp_relative_pointer_manager_v1_get_relative_pointer(relative_manager,pointer);zwp_relative_pointer_v1_add_listener(relative,&relative_listener,NULL);}
    printf("GLOBALS:relative=%d constraints=%d\n",relative_manager!=NULL,constraints!=NULL);fflush(stdout);
    char path[]="/tmp/mcp-surface-XXXXXX";int fd=mkstemp(path);unlink(path);const int width=80,height=60;ftruncate(fd,width*height*4);
    uint32_t *pixels=mmap(NULL,width*height*4,PROT_READ|PROT_WRITE,MAP_SHARED,fd,0);for(int i=0;i<width*height;i++)pixels[i]=0x80000080;
    struct wl_shm_pool *pool=wl_shm_create_pool(shm,fd,width*height*4);struct wl_buffer *buffer=wl_shm_pool_create_buffer(pool,0,width,height,width*4,WL_SHM_FORMAT_ARGB8888);wl_shm_pool_destroy(pool);close(fd);munmap(pixels,width*height*4);
    child=wl_compositor_create_surface(compositor);struct wl_subsurface *sub=wl_subcompositor_get_subsurface(subcompositor,child,surface);wl_subsurface_set_position(sub,20,120);
    struct wl_region *empty=wl_compositor_create_region(compositor);wl_surface_set_input_region(child,empty);wl_region_destroy(empty);
    wl_surface_attach(child,buffer,0,0);wl_surface_damage(child,0,0,width,height);wl_surface_commit(child);wl_surface_commit(surface);wl_display_flush(display);gtk_widget_queue_draw(GTK_WIDGET(window));return G_SOURCE_REMOVE;
}
static void drag_begin(GtkDragSource *source,GdkDrag *drag,void *unused){(void)source;(void)drag;(void)unused;puts("DRAG_BEGIN");fflush(stdout);}
static gboolean drop_text(GtkDropTarget *target,const GValue *value,double x,double y,void *unused){(void)target;(void)x;(void)y;(void)unused;char text[256];snprintf(text,sizeof text,"DROP:%s",g_value_get_string(value));report(text);return TRUE;}
int main(void){gtk_init();GtkWidget *window=gtk_window_new();gtk_window_set_title(GTK_WINDOW(window),"MCP input and subsurface compositor");gtk_window_set_default_size(GTK_WINDOW(window),500,400);
    GtkWidget *box=gtk_box_new(GTK_ORIENTATION_VERTICAL,20);gtk_widget_set_margin_top(box,20);gtk_widget_set_margin_start(box,20);gtk_widget_set_margin_end(box,20);
    gtk_box_append(GTK_BOX(box),gtk_label_new("Blue translucent area is a native wl_subsurface"));
    GtkWidget *row=gtk_box_new(GTK_ORIENTATION_HORIZONTAL,20);GtkWidget *lock=gtk_button_new_with_label("Lock pointer (5 seconds)");GtkWidget *confine=gtk_button_new_with_label("Confine pointer (5 seconds)");g_signal_connect(lock,"clicked",G_CALLBACK(capture),NULL);g_signal_connect(confine,"clicked",G_CALLBACK(capture),(void*)1);gtk_box_append(GTK_BOX(row),lock);gtk_box_append(GTK_BOX(row),confine);gtk_box_append(GTK_BOX(box),row);
    status=gtk_label_new("Relative motion and touch appear here");gtk_box_append(GTK_BOX(box),status);GtkWidget *drag_label=gtk_label_new("Drag this private text");gtk_widget_set_size_request(drag_label,300,60);gtk_box_append(GTK_BOX(box),drag_label);
    GtkDragSource *drag_source=gtk_drag_source_new();gtk_drag_source_set_actions(drag_source,GDK_ACTION_COPY);
    GdkContentProvider *content=gdk_content_provider_new_typed(G_TYPE_STRING,"PRIVATE DND: visible GTK");gtk_drag_source_set_content(drag_source,content);g_object_unref(content);g_signal_connect(drag_source,"drag-begin",G_CALLBACK(drag_begin),NULL);gtk_widget_add_controller(drag_label,GTK_EVENT_CONTROLLER(drag_source));
    GtkWidget *drop_label=gtk_label_new("Drop private text here");gtk_widget_set_size_request(drop_label,300,60);gtk_box_append(GTK_BOX(box),drop_label);
    GtkDropTarget *drop_target=gtk_drop_target_new(G_TYPE_STRING,GDK_ACTION_COPY);g_signal_connect(drop_target,"drop",G_CALLBACK(drop_text),NULL);gtk_widget_add_controller(drop_label,GTK_EVENT_CONTROLLER(drop_target));
    GtkWidget *menu=gtk_menu_button_new();gtk_menu_button_set_label(GTK_MENU_BUTTON(menu),"Open native popup");
    GtkWidget *popover=gtk_popover_new();gtk_popover_set_child(GTK_POPOVER(popover),gtk_label_new("Native popup included in the window capture"));gtk_menu_button_set_popover(GTK_MENU_BUTTON(menu),popover);gtk_box_append(GTK_BOX(box),menu);
    gtk_window_set_child(GTK_WINDOW(window),box);
    GtkEventController *events=gtk_event_controller_legacy_new();g_signal_connect(events,"event",G_CALLBACK(legacy),NULL);gtk_widget_add_controller(window,events);
    gtk_window_present(GTK_WINDOW(window));g_timeout_add(50,native_setup,window);GMainLoop *loop=g_main_loop_new(NULL,FALSE);g_main_loop_run(loop);return 0;}
