extern int ext_v;
static int loc_v = 5;
int get(void) {
  return ext_v + loc_v;
}
