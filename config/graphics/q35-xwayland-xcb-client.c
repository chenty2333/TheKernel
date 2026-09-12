#include <errno.h>
#include <poll.h>
#include <time.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <xcb/xcb.h>

static int checked(xcb_connection_t *connection, xcb_void_cookie_t cookie,
                   const char *operation)
{
    xcb_generic_error_t *error = xcb_request_check(connection, cookie);
    if (error == NULL)
        return 0;
    fprintf(stderr, "THEKERNEL_Q35_XWAYLAND_GLAMOR_READY state=FAIL reason=%s code=%u\n",
            operation, error->error_code);
    free(error);
    return -1;
}

static int wait_viewable(xcb_connection_t *connection, xcb_window_t window)
{
    struct timespec start, now;
    struct pollfd descriptor = { .fd = xcb_get_file_descriptor(connection), .events = POLLIN };
    int mapped = 0;
    if (clock_gettime(CLOCK_MONOTONIC, &start) < 0)
        goto fail;
    xcb_flush(connection);
    for (;;) {
        xcb_generic_event_t *event;
        while ((event = xcb_poll_for_event(connection)) != NULL) {
            if ((event->response_type & 0x7f) == XCB_MAP_NOTIFY &&
                ((xcb_map_notify_event_t *)event)->window == window)
                mapped = 1;
            free(event);
        }
        if (mapped) {
            xcb_get_window_attributes_reply_t *attributes = xcb_get_window_attributes_reply(
                connection, xcb_get_window_attributes(connection, window), NULL);
            int viewable = attributes && attributes->map_state == XCB_MAP_STATE_VIEWABLE;
            free(attributes);
            if (viewable)
                return 0;
        }
        if (xcb_connection_has_error(connection) || clock_gettime(CLOCK_MONOTONIC, &now) < 0)
            goto fail;
        long elapsed = (now.tv_sec - start.tv_sec) * 1000 +
            (now.tv_nsec - start.tv_nsec) / 1000000;
        if (elapsed >= 5000)
            goto fail;
        /* A WM may map the client before its parent; recheck viewability too. */
        int timeout = (int)(5000 - elapsed);
        if (timeout > 50)
            timeout = 50;
        int ready = poll(&descriptor, 1, timeout);
        if (ready < 0 && errno != EINTR)
            goto fail;
        if (ready > 0 && (descriptor.revents & (POLLERR | POLLHUP | POLLNVAL)))
            goto fail;
    }
fail:
    fputs("THEKERNEL_Q35_XWAYLAND_GLAMOR_READY state=FAIL reason=map_viewable\n", stderr);
    return -1;
}

static xcb_atom_t atom(xcb_connection_t *connection, const char *name)
{
    xcb_intern_atom_reply_t *reply = xcb_intern_atom_reply(
        connection, xcb_intern_atom(connection, 0, (uint16_t)strlen(name), name), NULL);
    xcb_atom_t result = reply ? reply->atom : XCB_ATOM_NONE;
    free(reply);
    return result;
}

static int clipboard_roundtrip(xcb_connection_t *owner, xcb_window_t source,
                              xcb_atom_t clipboard, xcb_atom_t utf8)
{
    xcb_connection_t *reader = xcb_connect(NULL, NULL);
    xcb_atom_t property = atom(owner, "THEKERNEL_CLIPBOARD_RESULT");
    xcb_atom_t targets = atom(owner, "TARGETS");
    struct timespec start, now;
    int result = -1;
    if (!reader || xcb_connection_has_error(reader) || !property || !targets)
        goto done;
    xcb_screen_t *screen = xcb_setup_roots_iterator(xcb_get_setup(reader)).data;
    xcb_window_t window = xcb_generate_id(reader);
    if (checked(reader, xcb_create_window_checked(reader, XCB_COPY_FROM_PARENT,
                window, screen->root, 0, 0, 1, 1, 0, XCB_WINDOW_CLASS_INPUT_OUTPUT,
                screen->root_visual, 0, NULL), "clipboard_reader") < 0 ||
        checked(reader, xcb_convert_selection_checked(reader, window, clipboard,
                utf8, property, XCB_CURRENT_TIME), "clipboard_request") < 0 ||
        clock_gettime(CLOCK_MONOTONIC, &start) < 0)
        goto done;
    xcb_flush(reader);
    struct pollfd descriptors[2] = {
        { .fd = xcb_get_file_descriptor(owner), .events = POLLIN },
        { .fd = xcb_get_file_descriptor(reader), .events = POLLIN },
    };
    for (;;) {
        xcb_generic_event_t *event;
        while ((event = xcb_poll_for_event(owner)) != NULL) {
            if ((event->response_type & 0x7f) == XCB_SELECTION_REQUEST) {
                xcb_selection_request_event_t *request = (void *)event;
                xcb_selection_notify_event_t response = {
                    .response_type = XCB_SELECTION_NOTIFY,
                    .time = request->time, .requestor = request->requestor,
                    .selection = request->selection, .target = request->target,
                    .property = XCB_ATOM_NONE,
                };
                if (request->owner == source && request->selection == clipboard) {
                    xcb_atom_t destination = request->property ? request->property : request->target;
                    if (request->target == utf8) {
                        xcb_change_property(owner, XCB_PROP_MODE_REPLACE, request->requestor,
                                destination, utf8, 8, 9, "TheKernel");
                        response.property = destination;
                    } else if (request->target == targets) {
                        xcb_atom_t supported[] = { targets, utf8 };
                        xcb_change_property(owner, XCB_PROP_MODE_REPLACE, request->requestor,
                                destination, XCB_ATOM_ATOM, 32, 2, supported);
                        response.property = destination;
                    }
                }
                char wire_event[32] = { 0 };
                memcpy(wire_event, &response, sizeof(response));
                xcb_send_event(owner, 0, request->requestor, XCB_EVENT_MASK_NO_EVENT, wire_event);
                xcb_flush(owner);
            }
            free(event);
        }
        while ((event = xcb_poll_for_event(reader)) != NULL) {
            if ((event->response_type & 0x7f) == XCB_SELECTION_NOTIFY) {
                xcb_selection_notify_event_t *notify = (void *)event;
                int matches = notify->requestor == window && notify->selection == clipboard &&
                    notify->target == utf8 && notify->property == property;
                free(event);
                if (!matches)
                    goto done;
                xcb_get_property_reply_t *value = xcb_get_property_reply(reader,
                        xcb_get_property(reader, 1, window, property, utf8, 0, 16), NULL);
                if (value && value->type == utf8 && value->format == 8 &&
                    value->bytes_after == 0 && xcb_get_property_value_length(value) == 9 &&
                    memcmp(xcb_get_property_value(value), "TheKernel", 9) == 0)
                    result = 0;
                free(value);
                goto done;
            }
            free(event);
        }
        if (xcb_connection_has_error(owner) || xcb_connection_has_error(reader) ||
            clock_gettime(CLOCK_MONOTONIC, &now) < 0)
            goto done;
        long elapsed = (now.tv_sec - start.tv_sec) * 1000 +
            (now.tv_nsec - start.tv_nsec) / 1000000;
        if (elapsed >= 5000)
            goto done;
        int ready = poll(descriptors, 2, (int)(5000 - elapsed));
        if (ready < 0 && errno != EINTR)
            goto done;
        if ((descriptors[0].revents | descriptors[1].revents) & (POLLERR | POLLHUP | POLLNVAL))
            goto done;
    }
done:
    if (reader)
        xcb_disconnect(reader);
    if (result < 0)
        fputs("THEKERNEL_Q35_XWAYLAND_GLAMOR_READY state=FAIL reason=clipboard_transfer\n", stderr);
    return result;
}

int main(void)
{
    xcb_connection_t *connection = xcb_connect(NULL, NULL);
    const xcb_setup_t *setup;
    xcb_screen_iterator_t screens;
    xcb_screen_t *screen;
    xcb_window_t background, foreground;
    xcb_gcontext_t gc;
    xcb_atom_t clipboard, utf8_string;
    uint32_t window_values[2], stack_mode = XCB_STACK_MODE_ABOVE;
    uint32_t resize_values[2] = { 360, 260 }, color = 0xff2040;
    struct pollfd pollfd;
    int saw_key = 0, saw_pointer = 0;

    if (!connection || xcb_connection_has_error(connection)) {
        fputs("THEKERNEL_Q35_XWAYLAND_GLAMOR_READY state=FAIL reason=xcb_connect\n", stderr);
        return 1;
    }
    setup = xcb_get_setup(connection);
    screens = xcb_setup_roots_iterator(setup);
    if (!screens.rem) {
        fputs("THEKERNEL_Q35_XWAYLAND_GLAMOR_READY state=FAIL reason=xcb_screen\n", stderr);
        xcb_disconnect(connection);
        return 1;
    }
    screen = screens.data;
    background = xcb_generate_id(connection);
    foreground = xcb_generate_id(connection);
    gc = xcb_generate_id(connection);
    window_values[0] = screen->black_pixel;
    window_values[1] = XCB_EVENT_MASK_EXPOSURE | XCB_EVENT_MASK_KEY_PRESS |
        XCB_EVENT_MASK_BUTTON_PRESS | XCB_EVENT_MASK_POINTER_MOTION | XCB_EVENT_MASK_FOCUS_CHANGE |
        XCB_EVENT_MASK_STRUCTURE_NOTIFY;
    if (checked(connection, xcb_create_window_checked(connection, XCB_COPY_FROM_PARENT,
                background, screen->root, 64, 64, 320, 220, 0, XCB_WINDOW_CLASS_INPUT_OUTPUT,
                screen->root_visual, XCB_CW_BACK_PIXEL | XCB_CW_EVENT_MASK, window_values), "map_background") < 0 ||
        checked(connection, xcb_create_window_checked(connection, XCB_COPY_FROM_PARENT,
                foreground, screen->root, 128, 112, 280, 180, 0, XCB_WINDOW_CLASS_INPUT_OUTPUT,
                screen->root_visual, XCB_CW_BACK_PIXEL | XCB_CW_EVENT_MASK, window_values), "map_foreground") < 0 ||
        checked(connection, xcb_create_gc_checked(connection, gc, foreground, XCB_GC_FOREGROUND, &color), "create_gc") < 0 ||
        checked(connection, xcb_poly_fill_rectangle_checked(connection, foreground, gc, 1,
                &(xcb_rectangle_t) { .x = 0, .y = 0, .width = 280, .height = 180 }), "draw") < 0 ||
        checked(connection, xcb_map_window_checked(connection, background), "map_background") < 0 ||
        checked(connection, xcb_map_window_checked(connection, foreground), "map_foreground") < 0 ||
        checked(connection, xcb_configure_window_checked(connection, foreground,
                XCB_CONFIG_WINDOW_WIDTH | XCB_CONFIG_WINDOW_HEIGHT, resize_values), "resize") < 0 ||
        checked(connection, xcb_configure_window_checked(connection, foreground,
                XCB_CONFIG_WINDOW_STACK_MODE, &stack_mode), "raise_layer") < 0 ||
        wait_viewable(connection, foreground) < 0 ||
        checked(connection, xcb_set_input_focus_checked(connection, XCB_INPUT_FOCUS_POINTER_ROOT,
                foreground, XCB_CURRENT_TIME), "focus") < 0)
        goto fail;
    clipboard = atom(connection, "CLIPBOARD");
    utf8_string = atom(connection, "UTF8_STRING");
    if (clipboard == XCB_ATOM_NONE || utf8_string == XCB_ATOM_NONE ||
        checked(connection, xcb_change_property_checked(connection, XCB_PROP_MODE_REPLACE,
                foreground, utf8_string, utf8_string, 8, 9, "TheKernel"), "clipboard_data") < 0 ||
        checked(connection, xcb_set_selection_owner_checked(connection, foreground,
                clipboard, XCB_CURRENT_TIME), "clipboard_owner") < 0)
        goto fail;
    xcb_flush(connection);
    xcb_get_selection_owner_reply_t *owner = xcb_get_selection_owner_reply(
        connection, xcb_get_selection_owner(connection, clipboard), NULL);
    if (!owner || owner->owner != foreground) {
        free(owner);
        fputs("THEKERNEL_Q35_XWAYLAND_GLAMOR_READY state=FAIL reason=clipboard_owner\n", stderr);
        goto fail;
    }
    free(owner);
    if (clipboard_roundtrip(connection, foreground, clipboard, utf8_string) < 0)
        goto fail;
    puts("THEKERNEL_Q35_XWAYLAND_EVENT_READY");
    fflush(stdout);
    pollfd.fd = xcb_get_file_descriptor(connection);
    pollfd.events = POLLIN;
    while (poll(&pollfd, 1, 5000) > 0) {
        xcb_generic_event_t *event;
        while ((event = xcb_poll_for_event(connection)) != NULL) {
            switch (event->response_type & 0x7f) {
            case XCB_KEY_PRESS: saw_key = 1; break;
            case XCB_BUTTON_PRESS:
            case XCB_MOTION_NOTIFY: saw_pointer = 1; break;
            default: break;
            }
            free(event);
        }
        if (saw_key && saw_pointer)
            break;
    }
    if (saw_key)
        puts("THEKERNEL_Q35_XWAYLAND_KEY_EVENT");
    if (saw_pointer)
        puts("THEKERNEL_Q35_XWAYLAND_POINTER_EVENT");
    xcb_disconnect(connection);
    return 0;
fail:
    xcb_disconnect(connection);
    return 1;
}
