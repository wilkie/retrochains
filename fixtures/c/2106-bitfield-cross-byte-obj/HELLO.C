struct Cross { unsigned lo : 6; unsigned mid : 6; unsigned hi : 4; };
int main(void) {
  struct Cross c;
  c.lo = 30;
  c.mid = 40;
  c.hi = 5;
  return (int)c.lo + (int)c.mid + (int)c.hi;
}
