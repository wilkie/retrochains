struct M { int tag; char flag; };
struct M m;
int get_flag(void) {
  return m.flag;
}
