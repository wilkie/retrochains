int g = 100;
int *gp = &g;
int main(void) {
  *gp &= 12;
  return g;
}
