struct S { int x; } s1, s2;

int pick(int c) {
  return (c ? &s1 : &s2)->x;
}
