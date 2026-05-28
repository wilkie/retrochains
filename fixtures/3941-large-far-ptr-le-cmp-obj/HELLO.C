int g1, g2;
int main(void) {
  int *p1 = &g1;
  int *p2 = &g2;
  if (p1 <= p2) {
    return 1;
  }
  return 0;
}
