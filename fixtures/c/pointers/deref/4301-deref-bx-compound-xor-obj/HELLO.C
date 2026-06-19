int g = 100;
int *gp = &g;
int main(void) {
  *gp ^= 6;
  return g;
}
