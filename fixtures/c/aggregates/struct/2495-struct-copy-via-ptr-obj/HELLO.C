struct Pair { int a; int b; };
void copy(struct Pair *dst, struct Pair *src) {
  *dst = *src;
}
