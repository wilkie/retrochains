struct H { char tag; char ver; int val; };
struct H rec = { 'X', 1, 42 };
int main(void) {
  return rec.val;
}
