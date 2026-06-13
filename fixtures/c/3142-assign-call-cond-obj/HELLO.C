int get_val(void);
int handle(void) {
  int x;
  if ((x = get_val()) != 0) return x;
  return -1;
}
