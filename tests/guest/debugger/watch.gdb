set pagination off
set confirm off
break main
run
watch watched
continue
if watched != 7
  quit 1
end
printf "WATCH_FIRST_OK\n"
continue
if watched != 19
  quit 1
end
printf "WATCH_SECOND_OK\n"
continue
