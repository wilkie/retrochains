int g = 100;
int *gp = &g;
int main(void) {
  *gp |= 8;
  return g;
}
