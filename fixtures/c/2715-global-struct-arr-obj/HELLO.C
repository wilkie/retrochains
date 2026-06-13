struct E { int code; int weight; };
struct E table[3] = {
  { 10, 100 },
  { 20, 200 },
  { 30, 300 }
};
int main(void) {
  return table[2].weight;
}
